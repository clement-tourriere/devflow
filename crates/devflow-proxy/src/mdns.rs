//! mDNS/Bonjour responder.
//!
//! Advertises one `A` record per discovered service so that friendly `.local`
//! names resolve **from the host** with no `/etc/hosts` and no `/etc/resolver`
//! edits. Records are registered through the system DNS-SD daemon (Apple's
//! `mDNSResponder` on macOS), so the daemon is authoritative and resolution is
//! reliable — no multicast race with a second responder.
//!
//! The IP a name resolves to depends on the service:
//!
//! - **HTTP services → `127.0.0.1`** so the request lands on the proxy's HTTPS
//!   listener and is terminated with the trusted CA cert (`web.app.local`).
//! - **Direct-endpoint services (databases, caches, …) → the container IP** so
//!   the client connects straight to the container at its native port, with no
//!   proxy in the path (`postgresql://postgres.app.local:5432`). This relies on
//!   container IPs being routable from the host (OrbStack / Colima / Linux).
//!
//! Only macOS is wired up today; other platforms get a no-op responder that logs
//! a warning (their `.local` names will not resolve from the host).

use crate::discovery::ProxyTarget;
use crate::endpoint;
use std::net::Ipv4Addr;

/// Cross-platform handle the proxy uses to keep `.local` records in sync with
/// the set of discovered containers. The platform-specific machinery lives in
/// the `imp` module.
pub struct MdnsResponder {
    inner: imp::Responder,
}

impl MdnsResponder {
    /// Start the responder. Spawns the platform registrar (a dedicated thread on
    /// macOS). Cheap and infallible — failures degrade to "names won't resolve"
    /// and are logged, never fatal to the proxy.
    pub fn new() -> Self {
        Self {
            inner: imp::Responder::new(),
        }
    }

    /// The IP a target's name should resolve to: the container IP for direct
    /// (TCP database) endpoints, otherwise loopback (the HTTPS proxy). Returns
    /// `None` if the container IP is missing or not IPv4 (only A records today).
    fn target_ip(target: &ProxyTarget) -> Option<Ipv4Addr> {
        let ip_str = if endpoint::is_direct_endpoint_port(target.port) {
            target.container_ip.as_str()
        } else {
            "127.0.0.1"
        };
        ip_str.parse::<Ipv4Addr>().ok()
    }

    /// Advertise (or update) the A record for a single target.
    pub fn advertise(&self, target: &ProxyTarget) {
        match Self::target_ip(target) {
            Some(ip) => self
                .inner
                .advertise(&target.domain, ip, &target.container_id),
            None => log::debug!(
                "mDNS: skipping {} (no IPv4 address: {:?})",
                target.domain,
                target.container_ip
            ),
        }
    }

    /// Advertise a full set of targets (used for the initial discovery pass).
    pub fn reconcile(&self, targets: &[ProxyTarget]) {
        for target in targets {
            self.advertise(target);
        }
    }

    /// Withdraw every record belonging to a container (on stop/die).
    pub fn remove_by_container(&self, container_id: &str) {
        self.inner.remove_by_container(container_id);
    }
}

impl Default for MdnsResponder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// macOS: register A records via the system dns_sd API (Bonjour).
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::CString;
    use std::net::Ipv4Addr;
    use std::os::raw::{c_char, c_void};
    use std::ptr;
    use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
    use std::thread;

    /// FFI to the system DNS-SD library. The symbols live in libSystem on macOS,
    /// so no extra link directive is required.
    mod ffi {
        use std::os::raw::{c_char, c_void};

        pub type DnsServiceRef = *mut c_void;
        pub type DnsRecordRef = *mut c_void;
        pub type DnsServiceFlags = u32;
        pub type DnsServiceErrorType = i32;

        pub type DnsServiceRegisterRecordReply = Option<
            unsafe extern "C" fn(
                sd_ref: DnsServiceRef,
                record_ref: DnsRecordRef,
                flags: DnsServiceFlags,
                error_code: DnsServiceErrorType,
                context: *mut c_void,
            ),
        >;

        // Register the record as unique on the network (host A record).
        pub const FLAGS_UNIQUE: DnsServiceFlags = 0x20;
        pub const TYPE_A: u16 = 1;
        pub const CLASS_IN: u16 = 1;
        pub const INTERFACE_INDEX_ANY: u32 = 0;

        extern "C" {
            pub fn DNSServiceCreateConnection(sd_ref: *mut DnsServiceRef) -> DnsServiceErrorType;
            pub fn DNSServiceRefSockFD(sd_ref: DnsServiceRef) -> i32;
            pub fn DNSServiceProcessResult(sd_ref: DnsServiceRef) -> DnsServiceErrorType;
            pub fn DNSServiceRefDeallocate(sd_ref: DnsServiceRef);
            #[allow(clippy::too_many_arguments)]
            pub fn DNSServiceRegisterRecord(
                sd_ref: DnsServiceRef,
                record_ref: *mut DnsRecordRef,
                flags: DnsServiceFlags,
                interface_index: u32,
                fullname: *const c_char,
                rrtype: u16,
                rrclass: u16,
                rdlen: u16,
                rdata: *const c_void,
                ttl: u32,
                callback: DnsServiceRegisterRecordReply,
                context: *mut c_void,
            ) -> DnsServiceErrorType;
            pub fn DNSServiceRemoveRecord(
                sd_ref: DnsServiceRef,
                record_ref: DnsRecordRef,
                flags: DnsServiceFlags,
            ) -> DnsServiceErrorType;
        }
    }

    /// No-op callback — registration confirmations and conflict notifications are
    /// drained by the worker thread; we only log errors here.
    unsafe extern "C" fn register_reply(
        _sd_ref: ffi::DnsServiceRef,
        _record_ref: ffi::DnsRecordRef,
        _flags: ffi::DnsServiceFlags,
        error_code: ffi::DnsServiceErrorType,
        _context: *mut c_void,
    ) {
        if error_code != 0 {
            log::warn!("mDNS: record registration callback reported error {error_code}");
        }
    }

    enum Command {
        Advertise {
            name: String,
            ip: Ipv4Addr,
            container_id: String,
        },
        RemoveByContainer(String),
    }

    /// Handle held by the proxy. Commands are forwarded to the worker thread that
    /// owns the (non-`Send`) dns_sd connection. Dropping the handle closes the
    /// channel, which tears the worker down and withdraws every record.
    pub struct Responder {
        tx: Option<Sender<Command>>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl Responder {
        pub fn new() -> Self {
            let (tx, rx) = mpsc::channel::<Command>();
            let worker = thread::Builder::new()
                .name("devflow-mdns".to_string())
                .spawn(move || worker_loop(rx))
                .ok();
            if worker.is_none() {
                log::warn!("mDNS: failed to spawn responder thread; .local names will not resolve");
            }
            Self {
                tx: Some(tx),
                worker,
            }
        }

        pub fn advertise(&self, name: &str, ip: Ipv4Addr, container_id: &str) {
            if let Some(tx) = &self.tx {
                let _ = tx.send(Command::Advertise {
                    name: name.to_string(),
                    ip,
                    container_id: container_id.to_string(),
                });
            }
        }

        pub fn remove_by_container(&self, container_id: &str) {
            if let Some(tx) = &self.tx {
                let _ = tx.send(Command::RemoveByContainer(container_id.to_string()));
            }
        }
    }

    impl Drop for Responder {
        fn drop(&mut self) {
            // Closing the channel ends the worker loop, which deallocates the
            // shared connection and removes all advertised records.
            self.tx.take();
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    struct Record {
        container_id: String,
        name: String,
        record_ref: ffi::DnsRecordRef,
    }

    fn worker_loop(rx: Receiver<Command>) {
        // The connection is created on (and never leaves) this thread, so the
        // raw dns_sd pointers are only ever touched here.
        let mut shared: ffi::DnsServiceRef = ptr::null_mut();
        let err = unsafe { ffi::DNSServiceCreateConnection(&mut shared) };
        if err != 0 || shared.is_null() {
            log::warn!(
                "mDNS: DNSServiceCreateConnection failed ({err}); .local names will not resolve"
            );
            return;
        }
        let fd = unsafe { ffi::DNSServiceRefSockFD(shared) };
        let mut records: Vec<Record> = Vec::new();

        loop {
            // Drain all pending commands.
            loop {
                match rx.try_recv() {
                    Ok(Command::Advertise {
                        name,
                        ip,
                        container_id,
                    }) => advertise(shared, &mut records, name, ip, container_id),
                    Ok(Command::RemoveByContainer(id)) => {
                        remove_by_container(shared, &mut records, &id)
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        // Handle dropped: deallocating removes every record.
                        unsafe { ffi::DNSServiceRefDeallocate(shared) };
                        return;
                    }
                }
            }

            // Wait briefly for daemon replies (or the next command tick). The
            // 200ms timeout doubles as the command poll interval.
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let n = unsafe { libc::poll(&mut pfd, 1, 200) };
            if n > 0 && (pfd.revents & libc::POLLIN) != 0 {
                unsafe { ffi::DNSServiceProcessResult(shared) };
            }
        }
    }

    fn fqdn(name: &str) -> String {
        if name.ends_with('.') {
            name.to_string()
        } else {
            format!("{name}.")
        }
    }

    fn advertise(
        shared: ffi::DnsServiceRef,
        records: &mut Vec<Record>,
        name: String,
        ip: Ipv4Addr,
        container_id: String,
    ) {
        // Re-advertising a name (e.g. after a container restart changed its IP)
        // replaces the existing record.
        if let Some(pos) = records.iter().position(|r| r.name == name) {
            unsafe { ffi::DNSServiceRemoveRecord(shared, records[pos].record_ref, 0) };
            records.remove(pos);
        }

        let Ok(cname) = CString::new(fqdn(&name)) else {
            log::warn!("mDNS: invalid name {name:?}");
            return;
        };
        let octets = ip.octets();
        let mut record_ref: ffi::DnsRecordRef = ptr::null_mut();
        let err = unsafe {
            ffi::DNSServiceRegisterRecord(
                shared,
                &mut record_ref,
                ffi::FLAGS_UNIQUE,
                ffi::INTERFACE_INDEX_ANY,
                cname.as_ptr() as *const c_char,
                ffi::TYPE_A,
                ffi::CLASS_IN,
                octets.len() as u16,
                octets.as_ptr() as *const c_void,
                240,
                Some(register_reply),
                ptr::null_mut(),
            )
        };
        if err != 0 {
            log::warn!("mDNS: failed to advertise {name} -> {ip} (error {err})");
            return;
        }
        log::info!("mDNS: advertising {name} -> {ip}");
        records.push(Record {
            container_id,
            name,
            record_ref,
        });
    }

    fn remove_by_container(
        shared: ffi::DnsServiceRef,
        records: &mut Vec<Record>,
        container_id: &str,
    ) {
        records.retain(|r| {
            if r.container_id == container_id {
                unsafe { ffi::DNSServiceRemoveRecord(shared, r.record_ref, 0) };
                log::info!("mDNS: withdrawing {}", r.name);
                false
            } else {
                true
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Other platforms: no-op responder (names won't resolve from the host).
// ---------------------------------------------------------------------------
#[cfg(not(target_os = "macos"))]
mod imp {
    use std::net::Ipv4Addr;

    pub struct Responder;

    impl Responder {
        pub fn new() -> Self {
            log::warn!(
                "mDNS responder is only implemented on macOS; \
                 .local names will not resolve from this host"
            );
            Responder
        }

        pub fn advertise(&self, _name: &str, _ip: Ipv4Addr, _container_id: &str) {}

        pub fn remove_by_container(&self, _container_id: &str) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::ProxyTarget;

    fn target(domain: &str, ip: &str, port: u16) -> ProxyTarget {
        ProxyTarget {
            domain: domain.to_string(),
            container_ip: ip.to_string(),
            port,
            container_id: "abc123".to_string(),
            container_name: "c".to_string(),
            project: None,
            service: None,
            workspace: None,
        }
    }

    #[test]
    fn direct_endpoint_resolves_to_container_ip() {
        let t = target("postgres.app.local", "192.168.107.6", 5432);
        assert_eq!(
            MdnsResponder::target_ip(&t),
            Some("192.168.107.6".parse().unwrap())
        );
    }

    #[test]
    fn http_endpoint_resolves_to_loopback() {
        let t = target("web.app.local", "192.168.107.7", 3000);
        assert_eq!(MdnsResponder::target_ip(&t), Some(Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn missing_container_ip_is_skipped() {
        let t = target("postgres.app.local", "", 5432);
        assert_eq!(MdnsResponder::target_ip(&t), None);
    }
}
