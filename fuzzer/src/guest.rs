/// Driver for kernel to be tested
use crate::utils::cli::{App, Arg, OptVal};
use crate::Config;
use bytes::BytesMut;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use os_pipe::{pipe, PipeReader, PipeWriter};
use std::collections::HashMap;
use std::io::{ErrorKind, Read};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use tokio::process::Child;
use tokio::time::{delay_for, timeout, Duration};

lazy_static! {
    static ref QEMUS: HashMap<String, App> = {
        let mut qemus = HashMap::new();
        let linux_amd64_append_vals = vec![
            "earlyprintk=serial",
            "oops=panic",
            "nmi_watchdog=panic",
            "panic_on_warn=1",
            "panic=1",
            "ftrace_dump_on_oops=orig_cpu",
            "rodata=n",
            "vsyscall=native",
            "net.ifnames=0",
            "biosdevname=0",
            "root=/dev/sda",
            "console=ttyS0",
            "kvm-intel.nested=1",
            "kvm-intel.unrestricted_guest=1",
            "kvm-intel.vmm_exclusive=1",
            "kvm-intel.fasteoi=1",
            "kvm-intel.ept=1",
            "kvm-intel.flexpriority=1",
            "kvm-intel.vpid=1",
            "kvm-intel.emulate_invalid_guest_state=1",
            "kvm-intel.eptad=1",
            "kvm-intel.enable_shadow_vmcs=1",
            "kvm-intel.pml=1",
            "kvm-intel.enable_apicv=1",
        ];
        let linux_amd64 = App::new("qemu-system-x86_64")
            .arg(Arg::new_flag("-enable-kvm"))
            .arg(Arg::new_flag("-no-reboot"))
            .arg(Arg::new_opt("-display", OptVal::normal("none")))
            .arg(Arg::new_opt("-serial", OptVal::normal("stdio")))
            .arg(Arg::new_flag("-snapshot"))
            .arg(Arg::new_opt(
                "-cpu",
                OptVal::multiple(vec!["host", "migratable=off"], Some(',')),
            ))
            .arg(Arg::new_opt(
                "-net",
                OptVal::multiple(vec!["nic", "model=e1000"], Some(',')),
            ))
            .arg(Arg::new_opt(
                "-append",
                OptVal::multiple(linux_amd64_append_vals, Some(' ')),
            ));
        qemus.insert("linux/amd64".to_string(), linux_amd64);

        qemus
    };
    pub static ref SSH: App = {
        App::new("ssh")
            .arg(Arg::new_opt("-F", OptVal::normal("/dev/null")))
            .arg(Arg::new_opt(
                "-o",
                OptVal::normal("UserKnownHostsFile=/dev/null"),
            ))
            .arg(Arg::new_opt("-o", OptVal::normal("BatchMode=yes")))
            .arg(Arg::new_opt("-o", OptVal::normal("IdentitiesOnly=yes")))
            .arg(Arg::new_opt(
                "-o",
                OptVal::normal("StrictHostKeyChecking=no"),
            ))
            .arg(Arg::new_opt("-o", OptVal::normal("ConnectTimeout=3s")))
    };
    pub static ref SCP: App = {
        App::new("scp")
            .arg(Arg::new_opt("-F", OptVal::normal("/dev/null")))
            .arg(Arg::new_opt(
                "-o",
                OptVal::normal("UserKnownHostsFile=/dev/null"),
            ))
            .arg(Arg::new_opt("-o", OptVal::normal("BatchMode=yes")))
            .arg(Arg::new_opt("-o", OptVal::normal("IdentitiesOnly=yes")))
            .arg(Arg::new_opt(
                "-o",
                OptVal::normal("StrictHostKeyChecking=no"),
            ))
    };
}

#[derive(Debug, Deserialize)]
pub struct GuestConf {
    /// Kernel to be tested
    os: String,
    /// Arch of build kernel
    arch: String,
    /// Platform to run kernel, qemu or real env
    platform: String,
}

#[derive(Debug, Deserialize)]
pub struct QemuConf {
    pub cpu_num: u32,
    pub mem_size: u32,
    pub image: String,
    pub kernel: String,
    pub wait_boot_time: Option<u8>,
}

#[derive(Debug, Deserialize)]
pub struct SSHConf {
    pub key_path: String,
}

pub enum Guest {
    LinuxQemu(LinuxQemu),
}

impl Guest {
    pub fn new(cfg: &Config) -> Self {
        // only support linux/amd64 on qemu now.
        Guest::LinuxQemu(LinuxQemu::new(cfg))
    }
}

impl Guest {
    pub async fn boot(&mut self) {
        match self {
            Guest::LinuxQemu(ref mut guest) => guest.boot().await,
        }
    }

    pub async fn is_alive(&self) -> bool {
        match self {
            Guest::LinuxQemu(ref guest) => guest.is_alive().await,
        }
    }

    pub async fn run_cmd(&self, app: &App) -> Child {
        match self {
            Guest::LinuxQemu(ref guest) => guest.run_cmd(app).await,
        }
    }

    pub async fn try_collect_crash(&mut self) -> Option<String> {
        match self {
            Guest::LinuxQemu(ref mut guest) => guest.try_collect_crash().await,
        }
    }
}

pub const LINUX_QEMU_HOST_IP_ADDR: &str = "localhost";
pub const LINUX_QEMU_USER_NET_HOST_IP_ADDR: &str = "10.0.2.10";
pub const LINUX_QEMU_HOST_USER: &str = "root";
pub const LINUX_QEMU_PIPE_LEN: i32 = 1024 * 1024;

pub struct LinuxQemu {
    vm: App,
    wait_boot_time: u8,
    handle: Option<Child>,
    rp: Option<PipeReader>,

    addr: String,
    port: u16,
    key: String,
    user: String,
}

impl LinuxQemu {
    pub fn new(cfg: &Config) -> Self {
        assert_eq!(cfg.guest.platform.trim(), "qemu");
        assert_eq!(cfg.guest.os, "linux");
        assert_eq!(cfg.guest.arch, "amd64");

        let (qemu, port) = build_qemu_cli(&cfg);
        let ssh_conf = cfg
            .ssh
            .as_ref()
            .unwrap_or_else(|| exits!(exitcode::CONFIG, "Require ssh segment in config toml"));

        Self {
            vm: qemu,
            handle: None,
            rp: None,

            wait_boot_time: cfg.qemu.as_ref().unwrap().wait_boot_time.unwrap_or(5),
            addr: LINUX_QEMU_HOST_IP_ADDR.to_string(),
            port,
            key: ssh_conf.key_path.clone(),
            user: LINUX_QEMU_HOST_USER.to_string(),
        }
    }
}

impl LinuxQemu {
    async fn boot(&mut self) {
        const MAX_RETRY: u8 = 5;

        if let Some(ref mut h) = self.handle {
            h.kill()
                .unwrap_or_else(|e| exits!(exitcode::OSERR, "Fail to kill:{}", e));
            self.rp = None;
        }

        let (mut handle, mut rp) = {
            let mut cmd = self.vm.clone().into_cmd();
            let (rp, wp) = long_pipe();
            let wp2 = wp
                .try_clone()
                .unwrap_or_else(|e| exits!(exitcode::OSERR, "LinuxQemu: Fail to clone pipe:{}", e));
            let handle = cmd
                .stdin(std::process::Stdio::piped())
                .stdout(wp)
                .stderr(wp2)
                .kill_on_drop(true)
                .spawn()
                .unwrap_or_else(|e| exits!(exitcode::OSERR, "Fail to spawn qemu:{}", e));

            (handle, rp)
        };

        let mut retry = 1;
        loop {
            delay_for(Duration::new(self.wait_boot_time as u64, 0)).await;

            if self.is_alive().await {
                break;
            }

            if retry == MAX_RETRY {
                handle
                    .kill()
                    .unwrap_or_else(|e| exits!(exitcode::OSERR, "Fail to kill:{}", e));
                let mut buf = String::new();
                rp.read_to_string(&mut buf).unwrap_or_else(|e| {
                    exits!(exitcode::OSERR, "Fail to read to end of pipe:{}", e)
                });
                eprintln!("{}", buf);
                eprintln!("===============================================");
                exits!(exitcode::DATAERR, "Fail to boot :\n{:?}", self.vm);
            }
            retry += 1;
        }
        // clear useless data in pipe
        read_until_block(&mut rp);
        self.handle = Some(handle);
        self.rp = Some(rp);
    }

    async fn is_alive(&self) -> bool {
        let mut pwd = ssh_app(
            &self.key,
            &self.user,
            &self.addr,
            self.port,
            App::new("pwd"),
        )
        .into_cmd();
        pwd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        match timeout(Duration::new(10, 0), pwd.status()).await {
            Err(_) => false,
            Ok(status) => match status {
                Ok(status) => status.success(),
                Err(e) => exits!(exitcode::OSERR, "Fail to spawn:{}", e),
            },
        }
    }

    async fn run_cmd(&self, app: &App) -> Child {
        assert!(self.handle.is_some());

        let mut app = app.clone();
        let bin = PathBuf::from(app.bin);
        scp(&self.key, &self.user, &self.addr, self.port, &bin).await;

        app.bin = format!(
            "~/{}",
            bin.file_name()
                .unwrap_or_else(|| exits!(exitcode::DATAERR, "Bad app:{:?}", bin))
                .to_str()
                .unwrap()
        );

        let mut app = ssh_app(&self.key, &self.user, &self.addr, self.port, app).into_cmd();
        app.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap_or_else(|e| exits!(exitcode::OSERR, "Fail to spawn:{}", e))
    }

    async fn try_collect_crash(&mut self) -> Option<String> {
        assert!(self.rp.is_some());
        match timeout(Duration::new(2, 0), self.handle.as_mut().unwrap()).await {
            Err(_e) => None,
            Ok(_) => {
                self.handle = None;
                let mut crash_info = String::new();
                self.rp
                    .as_mut()
                    .unwrap()
                    .read_to_string(&mut crash_info)
                    .unwrap_or_else(|e| exits!(exitcode::OSERR, "Fail to read pipe of qemu:{}", e));
                self.rp = None;
                Some(crash_info)
            }
        }
    }
}

fn build_qemu_cli(cfg: &Config) -> (App, u16) {
    let target = format!("{}/{}", cfg.guest.os, cfg.guest.arch);

    let default_qemu = QEMUS
        .get(&target)
        .unwrap_or_else(|| exits!(exitcode::CONFIG, "Unsupported target:{}", &target))
        .clone();

    let port = port_check::free_local_port()
        .unwrap_or_else(|| exits!(exitcode::TEMPFAIL, "No Free port to forword"));
    let cfg = &cfg
        .qemu
        .as_ref()
        .unwrap_or_else(|| exits!(exitcode::SOFTWARE, "Require qemu segment in config toml"));
    let qemu = default_qemu
        .arg(Arg::new_opt("-m", OptVal::Normal(cfg.mem_size.to_string())))
        .arg(Arg::new_opt(
            "-smp",
            OptVal::Normal(cfg.cpu_num.to_string()),
        ))
        .arg(Arg::new_opt(
            "-net",
            OptVal::Multiple {
                vals: vec![
                    String::from("user"),
                    format!("host={}", LINUX_QEMU_USER_NET_HOST_IP_ADDR),
                    format!("hostfwd=tcp::{}-:22", port),
                ],
                sp: Some(','),
            },
        ))
        .arg(Arg::new_opt("-hda", OptVal::Normal(cfg.image.clone())))
        .arg(Arg::new_opt("-kernel", OptVal::Normal(cfg.kernel.clone())));
    (qemu, port)
}

fn ssh_app(key: &str, user: &str, addr: &str, port: u16, app: App) -> App {
    let mut ssh = SSH
        .clone()
        .arg(Arg::new_opt("-p", OptVal::normal(&port.to_string())))
        .arg(Arg::new_opt("-i", OptVal::normal(key)))
        .arg(Arg::Flag(format!("{}@{}", user, addr)))
        .arg(Arg::new_flag(&app.bin));
    for app_arg in app.iter_arg() {
        ssh = ssh.arg(Arg::Flag(app_arg));
    }
    ssh
}

async fn scp(key: &str, user: &str, addr: &str, port: u16, path: &PathBuf) {
    let scp = SCP
        .clone()
        .arg(Arg::new_opt("-P", OptVal::normal(&port.to_string())))
        .arg(Arg::new_opt("-i", OptVal::normal(key)))
        .arg(Arg::new_flag(path.as_path().to_str().unwrap()))
        .arg(Arg::Flag(format!("{}@{}:~/", user, addr)));

    let output = scp
        .into_cmd()
        .output()
        .await
        .unwrap_or_else(|e| panic!("Failed to spawn:{}", e));

    if !output.status.success() {
        panic!(String::from_utf8(output.stderr).unwrap())
    }
}

fn long_pipe() -> (PipeReader, PipeWriter) {
    let (rp, wp) = pipe().unwrap_or_else(|e| exits!(exitcode::OSERR, "Fail to creat pipe:{}", e));
    fcntl(wp.as_raw_fd(), FcntlArg::F_SETPIPE_SZ(1024 * 1024)).unwrap_or_else(|e| {
        exits!(
            exitcode::OSERR,
            "Fail to set pipe size to {} :{}",
            1024 * 1024,
            e
        )
    });
    fcntl(wp.as_raw_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK))
        .unwrap_or_else(|e| exits!(exitcode::OSERR, "Fail to set flag on pipe:{}", e));
    fcntl(rp.as_raw_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK))
        .unwrap_or_else(|e| exits!(exitcode::OSERR, "Fail to set flag on pipe:{}", e));

    (rp, wp)
}

fn read_until_block(rp: &mut PipeReader) -> BytesMut {
    const BUF_LEN: usize = 1024 * 1024;
    let mut result = BytesMut::with_capacity(BUF_LEN);
    unsafe {
        result.set_len(BUF_LEN);
    }

    let mut buf = &mut result[..];
    let mut count = 0;
    loop {
        match rp.read(buf) {
            Ok(n) => {
                assert_ne!(n, 0);
                count += n;
                buf = &mut buf[n..];
            }

            Err(e) => match e.kind() {
                ErrorKind::WouldBlock => break,
                _ => panic!(e),
            },
        }
    }
    unsafe {
        result.set_len(count);
    }
    result.truncate(count);
    result
}