extern crate clap;
extern crate console;
extern crate env_logger;
#[macro_use]
extern crate failure;
extern crate goblin;
extern crate indicatif;
#[macro_use]
extern crate lazy_static;
extern crate libc;
#[macro_use]
extern crate log;
extern crate proc_maps;
extern crate read_process_memory;
extern crate regex;
extern crate tempdir;
extern crate tempfile;
#[cfg(unix)]
extern crate termios;
#[cfg(windows)]
extern crate winapi;


mod binary_parser;
mod python_bindings;
mod python_interpreters;
mod python_spy;
mod stack_trace;
mod console_viewer;
mod flamegraph;

mod utils;

use std::vec::Vec;
use std::io::Read;

use clap::{App, Arg};
use failure::Error;

use python_spy::PythonSpy;
use stack_trace::StackTrace;
use console_viewer::ConsoleViewer;

fn print_traces(traces: &[StackTrace], show_idle: bool) {
    for trace in traces {
        if !show_idle && !trace.active {
            continue;
        }

        println!("Thread {:#X} ({})", trace.thread_id, trace.status_str());
        for frame in &trace.frames {
            let filename = match &frame.short_filename { Some(f) => &f, None => &frame.filename };
            println!("\t {} ({}:{})", frame.name, filename, frame.line);
        }
    }
}

// Given a failure::Error, tries to see if it is because the process exitted
fn process_exitted(err: &Error) -> bool {
    err.iter_chain().any(|cause| {
        if let Some(ioerror) = cause.downcast_ref::<std::io::Error>() {
            if let Some(err_code) = ioerror.raw_os_error() {
                if err_code == 3 || err_code == 60 || err_code == 299 {
                    return true;
                }
            }
        }
        false
    })
}

fn permission_denied(err: &Error) -> bool {
    err.iter_chain().any(|cause| {
        if let Some(ioerror) = cause.downcast_ref::<std::io::Error>() {
            ioerror.kind() == std::io::ErrorKind::PermissionDenied
        } else {
            false
        }
    })
}

fn sample_console(process: &PythonSpy,
                  display: &str,
                  show_idle: bool) -> Result<(), Error> {
    let rate = 10;
    let mut console = ConsoleViewer::new(show_idle, display,
                                         &format!("{}", process.version),
                                         rate as f64 / 1000.)?;
    let mut exitted_count = 0;
    loop {
        match process.get_stack_traces() {
            Ok(traces) => {
                console.increment(&traces)?;
            },
            Err(err) => {
                if process_exitted(&err) {
                    exitted_count += 1;
                    if exitted_count > 5 {
                        println!("\nprocess {} ended", process.pid);
                        break;
                    }
                } else {
                    console.increment_error(&err);
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(rate));
    }
    Ok(())
}


fn sample_flame(process: &PythonSpy, filename: &str) -> Result<(), Error> {
    let max_samples = 2000;
    let mut flame = flamegraph::Flamegraph::new();
    use indicatif::ProgressBar;
    let progress = ProgressBar::new(max_samples);

    println!("Taking {} samples of process", max_samples);
    let mut errors = 0;
    let mut samples = 0;
    let mut exitted_count = 0;
    for _ in 0..max_samples {
        match process.get_stack_traces() {
            Ok(traces) => {
                flame.increment(&traces)?;
                samples += 1;
            },
            Err(err) => {
                if process_exitted(&err) {
                    exitted_count += 1;
                    // there must be a better way to figure out if the process is still running
                    if exitted_count > 3 {
                        println!("process {} ended", process.pid);
                        break;
                    }
                }
                errors += 1;
            }
        }
        progress.inc(1);
    }
    progress.finish();

    let out_file = std::fs::File::create(filename)?;
    flame.write(out_file)?;
    println!("Wrote flame graph '{}'. Samples: {} Errors: {}", filename, samples, errors);

    // open generated flame graph in the browser on OSX (theory being that on linux
    // you might be SSH'ed into a server somewhere and this isn't desired, but on
    // that is pretty unlikely for osx) (note to self: xdg-open will open on linux)
    #[cfg(target_os = "macos")]
    std::process::Command::new("open").arg(filename).spawn()?;

    Ok(())
}

fn pyspy_main() -> Result<(), Error> {
    let matches = App::new("py-spy")
        .about("Spies on python programs!")
        .arg(Arg::with_name("pid")
            .short("p")
            .long("pid")
            .value_name("pid")
            .help("pid of python program to spy on")
            .takes_value(true)
            .required_unless("python_program"))
        .arg(Arg::with_name("dump")
            .short("d")
            .long("dump")
            .help("Dump the current stack traces to stdout"))
        .arg(Arg::with_name("flame")
            .short("f")
            .long("flame")
            .value_name("flame")
            .help("Generate a flame graph")
            .takes_value(true))
        .arg(Arg::with_name("python_program")
            .help("commandline of a python program to run")
            .multiple(true)
            )
        .get_matches();


    #[cfg(target_os="macos")]
    {
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("This program requires root on OSX.");
            eprintln!("Try running again with elevated permissions by going 'sudo !!'");
            std::process::exit(1)
        }
    }

    if let Some(pid_str) = matches.value_of("pid") {
        let pid: read_process_memory::Pid = pid_str.parse().expect("invalid pid");
        let process = PythonSpy::retry_new(pid, 3)?;

        if matches.occurrences_of("dump") > 0{
            print_traces(&process.get_stack_traces()?, true);
        } else if let Some(flame_file) = matches.value_of("flame") {
            sample_flame(&process, flame_file)?;
        } else {
            sample_console(&process, &format!("pid: {}", pid), false)?;
        }
    }

    else if let Some(subprocess) = matches.values_of("python_program") {
        // Dump out stdout/stderr from the process to a temp file, so we can view it later if needed
        let mut process_output = tempfile::NamedTempFile::new()?;
        let subprocess: Vec<&str> = subprocess.collect();
        let mut command = std::process::Command::new(subprocess[0])
            .args(&subprocess[1..])
            .stdin(std::process::Stdio::null())
            .stdout(process_output.reopen()?)
            .stderr(process_output.reopen()?)
            .spawn()?;

        #[cfg(target_os="macos")]
        {
            // sleep just in case: https://jvns.ca/blog/2018/01/28/mac-freeze/
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let result = match PythonSpy::retry_new(command.id() as read_process_memory::Pid, 8) {
            Ok(process) => {
                if let Some(flame_file) = matches.value_of("flame") {
                    sample_flame(&process, flame_file)
                } else {
                    sample_console(&process, &subprocess.join(" "), false)
                }
            },
            Err(e) => Err(e)
        };

        // check exit code of subprocess
        std::thread::sleep(std::time::Duration::from_millis(1));
        let success =  match command.try_wait()? {
            Some(exit) => exit.success(),
            // if process hasn't finished, assume success
            None => true
        };

        // if we failed for any reason, dump out stderr from child process here
        // (could have useful error message)
        if !success || result.is_err() {
            let mut buffer = String::new();
            if process_output.read_to_string(&mut buffer).is_ok() {
                eprintln!("{}", buffer);
            }
        }

        // kill it so we don't have dangling processess
        if command.kill().is_err() {
            // I don't actually care if we failed to kill ... most times process is already done
            // eprintln!("Error killing child process {}", e);
        }
        return result;
    }

    Ok(())
}

fn main() {
    env_logger::init();

    if let Err(err) = pyspy_main() {
        if permission_denied(&err) {
            eprintln!("Permission Denied: Try running again with elevated permissions by going 'sudo env \"PATH=$PATH\" !!'");
            std::process::exit(1);
        }

        eprintln!("Error: {}", err);
        for (i, suberror) in err.iter_chain().enumerate() {
            if i > 0 {
                eprintln!("Reason: {}", suberror);
            }
        }
        eprintln!("{}", err.backtrace());
        std::process::exit(1);
    }
}
