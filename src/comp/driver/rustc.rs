

// -*- rust -*-
import metadata::creader;
import front::parser;
import front::token;
import front::eval;
import front::ast;
import front::attr;
import middle::trans;
import middle::resolve;
import middle::ty;
import middle::typeck;
import middle::tstate::ck;
import pretty::pprust;
import pretty::ppaux;
import back::link;
import lib::llvm;
import util::common;
import std::fs;
import std::map::mk_hashmap;
import std::option;
import std::option::some;
import std::option::none;
import std::str;
import std::vec;
import std::io;
import std::run;
import std::getopts;
import std::getopts::optopt;
import std::getopts::optmulti;
import std::getopts::optflag;
import std::getopts::optflagopt;
import std::getopts::opt_present;
import back::link::output_type;

tag pp_mode { ppm_normal; ppm_typed; ppm_identified; }

fn default_configuration(session::session sess, str argv0, str input) ->
    ast::crate_cfg {
    auto libc =
        alt (sess.get_targ_cfg().os) {
            case (session::os_win32) { "msvcrt.dll" }
            case (session::os_macos) { "libc.dylib" }
            case (session::os_linux) { "libc.so.6" }
            case (_) { "libc.so" }
        };

    auto mk = attr::mk_name_value_item;

    ret [ // Target bindings.
         mk("target_os", std::os::target_os()),
         mk("target_arch", "x86"),
         mk("target_libc", libc),
         // Build bindings.
         mk("build_compiler", argv0),
         mk("build_input", input)];
}

fn build_configuration(session::session sess, str argv0,
                       str input) -> ast::crate_cfg {
    // Combine the configuration requested by the session (command line) with
    // some default configuration items
    ret sess.get_opts().cfg + default_configuration(sess, argv0, input);
}

// Convert strings provided as --cfg [cfgspec] into a crate_cfg
fn parse_cfgspecs(&vec[str] cfgspecs) -> ast::crate_cfg {
    // FIXME: It would be nice to use the parser to parse all varieties of
    // meta_item here. At the moment we just support the meta_word variant.
    fn to_meta_word(&str cfgspec) -> @ast::meta_item {
        attr::mk_word_item(cfgspec)
    }
    ret vec::map(to_meta_word, cfgspecs);
}

fn parse_input(session::session sess, parser::parser p, str input) ->
   @ast::crate {
    ret if (str::ends_with(input, ".rc")) {
            parser::parse_crate_from_crate_file(p)
        } else if (str::ends_with(input, ".rs")) {
            parser::parse_crate_from_source_file(p)
        } else { sess.fatal("unknown input file type: " + input); fail };
}

fn time[T](bool do_it, str what, fn() -> T  thunk) -> T {
    if (!do_it) { ret thunk(); }
    auto start = std::time::get_time();
    auto rv = thunk();
    auto end = std::time::get_time();
    // FIXME: Actually do timeval math.

    log_err #fmt("time: %s took %u s", what, end.sec - start.sec as uint);
    ret rv;
}

fn compile_input(session::session sess, ast::crate_cfg cfg, str input,
                 str output) {
    auto time_passes = sess.get_opts().time_passes;
    auto p = parser::new_parser(sess, cfg, input, 0u, 0);
    auto crate =
        time(time_passes, "parsing", bind parse_input(sess, p, input));
    if (sess.get_opts().output_type == link::output_type_none) { ret; }
    crate = time(time_passes, "configuration",
                 bind front::config::strip_unconfigured_items(crate));
    auto ast_map = time(time_passes, "ast indexing",
                        bind middle::ast_map::map_crate(*crate));
    auto d =
        time(time_passes, "resolution",
             bind resolve::resolve_crate(sess, ast_map, crate));
    auto ty_cx = ty::mk_ctxt(sess, d._0, d._1, ast_map);
    time[()](time_passes, "typechecking",
             bind typeck::check_crate(ty_cx, crate));
    if (sess.get_opts().run_typestate) {
        time(time_passes, "typestate checking",
             bind middle::tstate::ck::check_crate(ty_cx, crate));
    }
    time(time_passes, "alias checking",
         bind middle::alias::check_crate(@ty_cx, crate));
    auto llmod =
        time[llvm::llvm::ModuleRef](time_passes, "translation",
                                    bind trans::trans_crate
                                    (sess, crate, ty_cx, output, ast_map));
    time[()](time_passes, "LLVM passes",
             bind link::write::run_passes(sess, llmod, output));
}

fn pretty_print_input(session::session sess, ast::crate_cfg cfg,
                      str input, pp_mode ppm) {
    auto p = front::parser::new_parser(sess, cfg, input, 0u, 0);
    auto crate = parse_input(sess, p, input);
    auto mode;
    alt (ppm) {
        case (ppm_typed) {
            auto amap = middle::ast_map::map_crate(*crate);
            auto d = resolve::resolve_crate(sess, amap, crate);
            auto ty_cx = ty::mk_ctxt(sess, d._0, d._1, amap);
            typeck::check_crate(ty_cx, crate);
            mode = ppaux::mo_typed(ty_cx);
        }
        case (ppm_normal) { mode = ppaux::mo_untyped; }
        case (ppm_identified) { mode = ppaux::mo_identified; }
    }
    pprust::print_crate(sess, crate, input, std::io::stdout(),
                        mode);
}

fn version(str argv0) {
    auto vers = "unknown version";
    auto env_vers = #env("CFG_VERSION");
    if (str::byte_len(env_vers) != 0u) { vers = env_vers; }
    io::stdout().write_str(#fmt("%s %s\n", argv0, vers));
}

fn usage(str argv0) {
    io::stdout().write_str(#fmt("usage: %s [options] <input>\n", argv0) +
                               "
options:

    -h --help          display this message
    -v --version       print version info and exit

    -o <filename>      write output to <filename>
    --glue             generate glue.bc file
    --shared           compile a shared-library crate
    --pretty [type]    pretty-print the input instead of compiling
    --ls               list the symbols defined by a crate file
    -L <path>          add a directory to the library search path
    --noverify         suppress LLVM verification step (slight speedup)
    --depend           print dependencies, in makefile-rule form
    --parse-only       parse only; do not compile, assemble, or link
    -g                 produce debug info
    --OptLevel=        optimize with possible levels 0-3
    -O                 equivalent to --OptLevel=2
    -S                 compile only; do not assemble or link
    -c                 compile and assemble, but do not link
    --emit-llvm        produce an LLVM bitcode file
    --save-temps       write intermediate files in addition to normal output
    --stats            gather and report various compilation statistics
    --cfg [cfgspec]    configure the compilation environment
    --time-passes      time the individual phases of the compiler
    --time-llvm-passes time the individual phases of the LLVM backend
    --sysroot <path>   override the system root (default: rustc's directory)
    --no-typestate     don't run the typestate pass (unsafe!)\n\n");
}

fn get_os(str triple) -> session::os {
    ret if (str::find(triple, "win32") >= 0 ||
                str::find(triple, "mingw32") >= 0) {
            session::os_win32
        } else if (str::find(triple, "darwin") >= 0) {
            session::os_macos
        } else if (str::find(triple, "linux") >= 0) {
            session::os_linux
        } else { log_err "Unknown operating system!"; fail };
}

fn get_arch(str triple) -> session::arch {
    ret if (str::find(triple, "i386") >= 0 || str::find(triple, "i486") >= 0
                || str::find(triple, "i586") >= 0 ||
                str::find(triple, "i686") >= 0 ||
                str::find(triple, "i786") >= 0) {
            session::arch_x86
        } else if (str::find(triple, "x86_64") >= 0) {
            session::arch_x64
        } else if (str::find(triple, "arm") >= 0 ||
                       str::find(triple, "xscale") >= 0) {
            session::arch_arm
        } else { log_err "Unknown architecture! " + triple; fail };
}

fn get_default_sysroot(str binary) -> str {
    auto dirname = fs::dirname(binary);
    if (str::eq(dirname, binary)) { ret "."; }
    ret dirname;
}

fn build_target_config() -> @session::config {
    let str triple =
        std::str::rustrt::str_from_cstr(llvm::llvm::LLVMRustGetHostTriple());
    let @session::config target_cfg =
        @rec(os=get_os(triple),
             arch=get_arch(triple),
             int_type=common::ty_i32,
             uint_type=common::ty_u32,
             float_type=common::ty_f64);
    ret target_cfg;
}

fn build_session_options(str binary, getopts::match match, str binary_dir) ->
   @session::options {
    auto shared = opt_present(match, "shared");
    auto library_search_paths = [binary_dir + "/lib"];
    library_search_paths += getopts::opt_strs(match, "L");
    auto output_type =
        if (opt_present(match, "parse-only")) {
            link::output_type_none
        } else if (opt_present(match, "S")) {
            link::output_type_assembly
        } else if (opt_present(match, "c")) {
            link::output_type_object
        } else if (opt_present(match, "emit-llvm")) {
            link::output_type_bitcode
        } else { link::output_type_exe };
    auto verify = !opt_present(match, "noverify");
    auto save_temps = opt_present(match, "save-temps");
    auto debuginfo = opt_present(match, "g");
    auto stats = opt_present(match, "stats");
    auto time_passes = opt_present(match, "time-passes");
    auto time_llvm_passes = opt_present(match, "time-llvm-passes");
    auto run_typestate = !opt_present(match, "no-typestate");
    auto sysroot_opt = getopts::opt_maybe_str(match, "sysroot");
    let uint opt_level =
        if (opt_present(match, "O")) {
            if (opt_present(match, "OptLevel")) {
                log_err "error: -O and --OptLevel both provided";
                fail;
            }
            2u
        } else if (opt_present(match, "OptLevel")) {
            alt (getopts::opt_str(match, "OptLevel")) {
                case ("0") { 0u }
                case ("1") { 1u }
                case ("2") { 2u }
                case ("3") { 3u }
                case (_) {
                    log_err "error: optimization level needs " +
                                "to be between 0-3";
                    fail
                }
            }
        } else { 0u };
    auto sysroot =
        alt (sysroot_opt) {
            case (none) { get_default_sysroot(binary) }
            case (some(?s)) { s }
        };
    auto cfg = parse_cfgspecs(getopts::opt_strs(match, "cfg"));
    let @session::options sopts =
        @rec(shared=shared,
             optimize=opt_level,
             debuginfo=debuginfo,
             verify=verify,
             run_typestate=run_typestate,
             save_temps=save_temps,
             stats=stats,
             time_passes=time_passes,
             time_llvm_passes=time_llvm_passes,
             output_type=output_type,
             library_search_paths=library_search_paths,
             sysroot=sysroot,
             cfg=cfg);
    ret sopts;
}

fn build_session(@session::options sopts) -> session::session {
    auto target_cfg = build_target_config();
    auto crate_cache = common::new_int_hash[session::crate_metadata]();
    auto target_crate_num = 0;
    auto sess =
        session::session(target_crate_num, target_cfg, sopts, crate_cache, [],
                         [], front::codemap::new_codemap(), 0u);
    ret sess;
}

fn parse_pretty(session::session sess, &str name) -> pp_mode {
    if (str::eq(name, "normal")) {
        ret ppm_normal;
    } else if (str::eq(name, "typed")) {
        ret ppm_typed;
    } else if (str::eq(name, "identified")) { ret ppm_identified; }
    sess.fatal("argument to `pretty` must be one of `normal`, `typed`, or " +
                 "`identified`");
}

fn main(vec[str] args) {
    auto opts =
        [optflag("h"), optflag("help"), optflag("v"), optflag("version"),
         optflag("glue"), optflag("emit-llvm"), optflagopt("pretty"),
         optflag("ls"), optflag("parse-only"), optflag("O"),
         optopt("OptLevel"), optflag("shared"), optmulti("L"), optflag("S"),
         optflag("c"), optopt("o"), optflag("g"), optflag("save-temps"),
         optopt("sysroot"), optflag("stats"), optflag("time-passes"),
         optflag("time-llvm-passes"), optflag("no-typestate"),
         optflag("noverify"), optmulti("cfg")];
    auto binary = vec::shift[str](args);
    auto binary_dir = fs::dirname(binary);
    auto match =
        alt (getopts::getopts(args, opts)) {
            case (getopts::success(?m)) { m }
            case (getopts::failure(?f)) {
                log_err #fmt("error: %s", getopts::fail_str(f));
                fail
            }
        };
    if (opt_present(match, "h") || opt_present(match, "help")) {
        usage(binary);
        ret;
    }
    if (opt_present(match, "v") || opt_present(match, "version")) {
        version(binary);
        ret;
    }
    auto sopts = build_session_options(binary, match, binary_dir);
    auto sess = build_session(sopts);
    auto n_inputs = vec::len[str](match.free);
    auto output_file = getopts::opt_maybe_str(match, "o");
    auto glue = opt_present(match, "glue");
    if (glue) {
        if (n_inputs > 0u) {
            sess.fatal("No input files allowed with --glue.");
        }
        auto out = option::from_maybe[str]("glue.bc", output_file);
        middle::trans::make_common_glue(sess, out);
        ret;
    }
    if (n_inputs == 0u) {
        sess.fatal("No input filename given.");
    } else if (n_inputs > 1u) {
        sess.fatal("Multiple input filenames provided.");
    }
    auto ifile = match.free.(0);
    let str saved_out_filename = "";
    auto cfg = build_configuration(sess, binary, ifile);
    auto pretty =
        option::map[str,
                    pp_mode](bind parse_pretty(sess, _),
                             getopts::opt_default(match, "pretty", "normal"));
    auto ls = opt_present(match, "ls");
    alt (pretty) {
        case (some[pp_mode](?ppm)) {
            pretty_print_input(sess, cfg, ifile, ppm);
            ret;
        }
        case (none[pp_mode]) {/* continue */ }
    }
    if (ls) {
        metadata::creader::list_file_metadata(ifile, std::io::stdout());
        ret;
    }
    alt (output_file) {
        case (none) {
            let vec[str] parts = str::split(ifile, '.' as u8);
            vec::pop[str](parts);
            saved_out_filename = parts.(0);
            alt (sopts.output_type) {
                case (link::output_type_none) { parts += ["pp"]; }
                case (link::output_type_bitcode) { parts += ["bc"]; }
                case (link::output_type_assembly) { parts += ["s"]; }
                case (
                     // Object and exe output both use the '.o' extension here
                     link::output_type_object) {
                    parts += ["o"];
                }
                case (link::output_type_exe) { parts += ["o"]; }
            }
            auto ofile = str::connect(parts, ".");
            compile_input(sess, cfg, ifile, ofile);
        }
        case (some(?ofile)) {
            // FIXME: what about windows? This will create a foo.exe.o.

            saved_out_filename = ofile;
            auto temp_filename;
            alt (sopts.output_type) {
                case (link::output_type_exe) {
                    // FIXME: what about shared?

                    temp_filename = ofile + ".o";
                }
                case (_) { temp_filename = ofile; }
            }
            compile_input(sess, cfg, ifile, temp_filename);
        }
    }

    // If the user wants an exe generated we need to invoke
    // gcc to link the object file with some libs
    //
    // TODO: Factor this out of main.
    if (sopts.output_type == link::output_type_exe) {
        let str glu = binary_dir + "/lib/glue.o";
        let str main = "rt/main.o";
        let str stage = "-L" + binary_dir + "/lib";
        let str prog = "gcc";
        // The invocations of gcc share some flags across platforms

        let vec[str] gcc_args =
            [stage, "-Lrt", "-lrustrt", glu,  "-m32", "-o",
             saved_out_filename, saved_out_filename + ".o"];
        auto shared_cmd;

        alt (sess.get_targ_cfg().os) {
            case (session::os_win32) {
                shared_cmd = "-shared";
            }
            case (session::os_macos) {
                shared_cmd = "-dynamiclib";
            }
            case (session::os_linux) {
                shared_cmd = "-shared";
            }
        }

        // Converts a library file name into a gcc -l argument
        fn unlib(@session::config config, str filename) -> str {
            auto rmlib = bind fn(@session::config config,
                                 str filename) -> str {
                if (config.os == session::os_macos
                    || config.os == session::os_linux
                    && str::find(filename, "lib") == 0) {
                    ret str::slice(filename, 3u, str::byte_len(filename));
                } else {
                    ret filename;
                }
            } (config, _);
            fn rmext(str filename) -> str {
                auto parts = str::split(filename, '.' as u8);
                vec::pop(parts);
                ret str::connect(parts, ".");
            }
            ret alt (config.os) {
                case (session::os_macos) { rmext(rmlib(filename)) }
                case (session::os_linux) { rmext(rmlib(filename)) }
                case (_) { rmext(filename) }
            };
        }
        
        for (str cratepath in sess.get_used_crate_files()) {
            auto dir = fs::dirname(cratepath);
            if (dir != "") {
                gcc_args += ["-L" + dir];
            }
            auto libarg = unlib(sess.get_targ_cfg(), fs::basename(cratepath));
            gcc_args += ["-l" + libarg];
        }

        auto used_libs = sess.get_used_libraries();
        for (str l in used_libs) {
            gcc_args += ["-l" + l];
        }

        if (sopts.shared) {
            gcc_args += [shared_cmd];
        } else {
            // FIXME: having -Lrustllvm hardcoded in here is hack
            // FIXME: same for -lm
            gcc_args += ["-Lrustllvm", "-lm", main];
        }
        // We run 'gcc' here

        auto err_code = run::run_program(prog, gcc_args);
        if (0 != err_code) {
            sess.err(#fmt("linking with gcc failed with code %d", err_code));
            sess.note(#fmt("gcc arguments: %s", str::connect(gcc_args, " ")));
            sess.abort_if_errors();
        }
        // Clean up on Darwin

        if (sess.get_targ_cfg().os == session::os_macos) {
            run::run_program("dsymutil", [saved_out_filename]);
        }

        // Remove the temporary object file if we aren't saving temps
        if (!sopts.save_temps) {
            run::run_program("rm", [saved_out_filename + ".o"]);
        }
    }
}
// Local Variables:
// mode: rust
// fill-column: 78;
// indent-tabs-mode: nil
// c-basic-offset: 4
// buffer-file-coding-system: utf-8-unix
// compile-command: "make -k -C $RBUILD 2>&1 | sed -e 's/\\/x\\//x:\\//g'";
// End:
