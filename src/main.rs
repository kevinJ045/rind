use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::os::unix::io::FromRawFd;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use libc;
use nix::mount::{MsFlags, mount};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use once_cell::sync::Lazy;
use sdlang::parse_text;

struct Service {
    name: String,
    exec: String,
    args: Vec<String>,
    restart: bool,
    child: Option<Child>,
}

static SERVICES: Lazy<std::sync::Mutex<HashMap<String, Service>>> =
    Lazy::new(|| std::sync::Mutex::new(HashMap::new()));

fn load_sdl_file(path: &Path) -> Vec<sdlang::Tag> {
    let text = fs::read_to_string(path).unwrap_or_else(|_| {
        panic!("Failed to read SDLang file: {}", path.display());
    });

    let doc = parse_text(&text).unwrap_or_else(|e| {
        panic!("Failed to parse SDLang file {}: {}", path.display(), e);
    });

    let mut nodes = Vec::new();

    for node in doc.tags.into_iter() {
        if node.name == "import" {
            if let Some(import_path) = node.attr("path") {
                let import_path_string = &import_path.value.to_string();
                let import_path = Path::new(import_path_string);

                if import_path.to_string_lossy().contains('*') {
                    let parent = import_path.parent().unwrap();
                    let pattern = import_path.file_name().unwrap().to_string_lossy();

                    for entry in fs::read_dir(parent).unwrap() {
                        let entry = entry.unwrap();
                        let fname = entry.file_name().into_string().unwrap();
                        if glob::Pattern::new(&pattern).unwrap().matches(&fname) {
                            let mut sub_nodes = load_sdl_file(&entry.path());
                            nodes.append(&mut sub_nodes);
                        }
                    }
                } else {
                    let mut sub_nodes = load_sdl_file(import_path);
                    nodes.append(&mut sub_nodes);
                }
            }
        } else {
            nodes.push(node);
        }
    }

    nodes
}

fn load_services_from_sdl(path: &str) -> HashMap<String, Service> {
    let mut services = HashMap::new();
    let nodes = load_sdl_file(Path::new(path));

    for node in nodes {
        if node.name == "service" {
            let name = node
                .attr("name")
                .map_or("".to_string(), |x| x.value.to_string())
                .to_string();
            let exec = node
                .attr("exec")
                .map_or("".to_string(), |x| x.value.to_string())
                .to_string();
            let args: Vec<String> = node.tag("args").map_or(Vec::new(), |x| {
                x.attrs.iter().map(|x| x.value.to_string()).collect()
            });

            let restart = node
                .attr("restart")
                .map(|s| s.value.to_string() == "always")
                .unwrap_or(false);

            services.insert(
                name.clone(),
                Service {
                    name,
                    exec,
                    args,
                    restart,
                    child: None,
                },
            );
        }
    }

    services
}

fn spawn_service(service: &mut Service) {
    let child = Command::new(&service.exec)
        .args(&service.args)
        .spawn()
        .unwrap();

    println!("Started service {} with PID {}", service.name, child.id());
    service.child = Some(child);
}

fn spawn_tty(tty_path: &str) -> Option<Child> {
    let Ok(tty) = OpenOptions::new().read(true).write(true).open(tty_path) else {
        eprintln!("TTY file {tty_path} not found");
        return None;
    };

    let fd = tty.as_raw_fd();

    let stdin = unsafe { Stdio::from_raw_fd(fd) };
    let stdout = unsafe { Stdio::from_raw_fd(libc::dup(fd)) };
    let stderr = unsafe { Stdio::from_raw_fd(libc::dup(fd)) };

    match Command::new("/bin/sh")
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
    {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("Failed to start shell: {e}");
            None
        }
    }
}

fn main() {
    fs::create_dir_all("/proc").ok();
    fs::create_dir_all("/sys").ok();
    fs::create_dir_all("/dev").ok();

    mount(
        Some("proc"),
        "/proc",
        Some("proc"),
        MsFlags::empty(),
        None::<&str>,
    )
    .ok();
    mount(
        Some("sysfs"),
        "/sys",
        Some("sysfs"),
        MsFlags::empty(),
        None::<&str>,
    )
    .ok();
    mount(
        Some("devtmpfs"),
        "/dev",
        Some("devtmpfs"),
        MsFlags::empty(),
        None::<&str>,
    )
    .ok();

    let loaded_services = load_services_from_sdl("/etc/services.sd");
    let mut services = SERVICES.lock().unwrap();
    *services = loaded_services;

    let child = spawn_tty("/dev/ttyS0");

    if let Some(mut child) = child {
        child.wait().expect("Failed to wait for shell");
    }

    for service in services.values_mut() {
        spawn_service(service);
    }

    loop {
        match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, code)) => {
                println!("Child {} exited with code {}", pid, code);

                let mut services = SERVICES.lock().unwrap();
                for service in services.values_mut() {
                    if let Some(child) = &service.child {
                        if child.id() as i32 == pid.as_raw() {
                            service.child = None;
                            if service.restart {
                                println!("Restarting service {}", service.name);
                                spawn_service(service);
                            }
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(e) => eprintln!("waitpid error: {}", e),
        }

        thread::sleep(Duration::from_millis(100));
    }
}
