//! Narrow OpenZFS C API binding. Libraries are loaded only when ZFS is used,
//! so ordinary installs need neither ZFS headers nor a ZFS runtime.
//!
//! ABI: OpenZFS 2.x include/libzfs.h and include/sys/nvpair.h. Each operation
//! owns a libzfs handle on a blocking worker; handles never cross threads.
use anyhow::{anyhow, Context, Result};
use libloading::Library;
use std::{
    ffi::{c_char, c_int, c_uint, c_void, CStr, CString},
    path::{Path, PathBuf},
    ptr,
};

use super::replacement::{DatasetMutation, DatasetOps};

const FILESYSTEM: c_int = 1;
const SNAPSHOT: c_int = 2;
const VOLUME: c_int = 4;
const NOENT: c_int = 2009;
type Handle = *mut c_void;
type Callback = unsafe extern "C" fn(Handle, *mut c_void) -> c_int;

macro_rules! api {
    ($($name:ident: $ty:ty),* $(,)?) => {
        struct Api { _library: Library, $($name: $ty),* }
        impl Api {
            fn load() -> Result<Self> {
                anyhow::ensure!(cfg!(target_os = "linux"), "Native ZFS is supported on Linux only");
                let mut last = None;
                for name in ["libzfs.so.6", "libzfs.so.4", "libzfs.so.2", "libzfs.so"] {
                    // SAFETY: system dynamic loader resolves OpenZFS's public ABI;
                    // the library stays owned for the lifetime of every symbol.
                    match unsafe { Library::new(name) } {
                        Ok(library) => return unsafe { Ok(Self {
                            $($name: *library.get::<$ty>(concat!(stringify!($name), "\0").as_bytes())
                                .with_context(|| format!("OpenZFS library lacks {}", stringify!($name)))?),*,
                            _library: library,
                        }) },
                        Err(error) => last = Some(error),
                    }
                }
                Err(anyhow!("Cannot load OpenZFS libzfs: {}; install the system OpenZFS runtime", last.unwrap()))
            }
        }
    }
}

api! {
    libzfs_init: unsafe extern "C" fn() -> Handle,
    libzfs_fini: unsafe extern "C" fn(Handle),
    libzfs_errno: unsafe extern "C" fn(Handle) -> c_int,
    libzfs_error_description: unsafe extern "C" fn(Handle) -> *const c_char,
    zfs_open: unsafe extern "C" fn(Handle, *const c_char, c_int) -> Handle,
    zfs_close: unsafe extern "C" fn(Handle),
    zfs_get_name: unsafe extern "C" fn(Handle) -> *const c_char,
    zfs_get_type: unsafe extern "C" fn(Handle) -> c_int,
    zfs_name_to_prop: unsafe extern "C" fn(*const c_char) -> c_int,
    zfs_prop_get: unsafe extern "C" fn(Handle, c_int, *mut c_char, usize, *mut c_int, *mut c_char, usize, c_int) -> c_int,
    zfs_prop_set_list_flags: unsafe extern "C" fn(Handle, Handle, c_int) -> c_int,
    zfs_iter_root: unsafe extern "C" fn(Handle, Callback, *mut c_void) -> c_int,
    zfs_iter_children: unsafe extern "C" fn(Handle, Callback, *mut c_void) -> c_int,
    zfs_iter_filesystems: unsafe extern "C" fn(Handle, Callback, *mut c_void) -> c_int,
    zfs_create: unsafe extern "C" fn(Handle, *const c_char, c_int, Handle) -> c_int,
    zfs_clone: unsafe extern "C" fn(Handle, *const c_char, Handle) -> c_int,
    zfs_snapshot: unsafe extern "C" fn(Handle, *const c_char, c_int, Handle) -> c_int,
    zfs_destroy: unsafe extern "C" fn(Handle, c_int) -> c_int,
    lzc_rename: unsafe extern "C" fn(*const c_char, *const c_char) -> c_int,
    zfs_is_mounted: unsafe extern "C" fn(Handle, *mut *mut c_char) -> c_int,
    zfs_mount: unsafe extern "C" fn(Handle, *const c_char, c_int) -> c_int,
    nvlist_alloc: unsafe extern "C" fn(*mut Handle, c_uint, c_int) -> c_int,
    nvlist_free: unsafe extern "C" fn(Handle),
    nvlist_add_string: unsafe extern "C" fn(Handle, *const c_char, *const c_char) -> c_int,
}

struct Connection {
    api: Api,
    handle: Handle,
    // Keep the already loaded device open through the whole operation. Never
    // let libzfs_init's module loader invoke modprobe for an absent device.
    _device: std::fs::File,
}
impl Drop for Connection {
    fn drop(&mut self) {
        unsafe { (self.api.libzfs_fini)(self.handle) };
    }
}
struct Dataset<'a> {
    connection: &'a Connection,
    handle: Handle,
}
impl Drop for Dataset<'_> {
    fn drop(&mut self) {
        unsafe { (self.connection.api.zfs_close)(self.handle) };
    }
}
struct Properties<'a> {
    api: &'a Api,
    handle: Handle,
}
impl Drop for Properties<'_> {
    fn drop(&mut self) {
        unsafe { (self.api.nvlist_free)(self.handle) };
    }
}

impl Connection {
    fn new() -> Result<Self> {
        require_native_mounts(std::env::var_os("ZFS_MOUNT_HELPER").as_deref())?;
        anyhow::ensure!(
            Path::new("/sys/module/zfs").is_dir(),
            "ZFS kernel module is not loaded; provision ZFS outside devflow"
        );
        let device = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/zfs")
            .context(
                "Cannot open the existing /dev/zfs device; automatic module loading is disabled",
            )?;
        let api = Api::load()?;
        let handle = unsafe { (api.libzfs_init)() };
        anyhow::ensure!(
            !handle.is_null(),
            "Cannot initialize libzfs: {}; check /dev/zfs and permissions",
            std::io::Error::last_os_error()
        );
        Ok(Self {
            api,
            handle,
            _device: device,
        })
    }
    fn error(&self, action: &str) -> anyhow::Error {
        let detail = unsafe { CStr::from_ptr((self.api.libzfs_error_description)(self.handle)) }
            .to_string_lossy();
        anyhow!("ZFS {action}: {detail}")
    }
    fn check(&self, code: c_int, action: &str) -> Result<()> {
        if code == 0 {
            Ok(())
        } else {
            Err(self.error(action))
        }
    }
    fn open(&self, name: &str) -> Result<Option<Dataset<'_>>> {
        let name_c = CString::new(name)?;
        let handle = unsafe {
            (self.api.zfs_open)(self.handle, name_c.as_ptr(), FILESYSTEM | SNAPSHOT | VOLUME)
        };
        if !handle.is_null() {
            return Ok(Some(Dataset {
                connection: self,
                handle,
            }));
        }
        if unsafe { (self.api.libzfs_errno)(self.handle) } == NOENT {
            return Ok(None);
        }
        Err(self.error(&format!("open {name}")))
    }
    fn dataset(&self, name: &str) -> Result<Dataset<'_>> {
        self.open(name)?
            .with_context(|| format!("ZFS dataset '{name}' is missing"))
    }
    fn properties(&self, mountpoint: Option<&Path>) -> Result<Properties<'_>> {
        let mut handle = ptr::null_mut();
        let code = unsafe { (self.api.nvlist_alloc)(&mut handle, 1, 0) };
        anyhow::ensure!(
            code == 0 && !handle.is_null(),
            "Cannot allocate ZFS properties: {code}"
        );
        let props = Properties {
            api: &self.api,
            handle,
        };
        let value = mountpoint
            .map(|p| p.as_os_str().as_encoded_bytes())
            .unwrap_or(b"none");
        let value = CString::new(value)?;
        let code =
            unsafe { (self.api.nvlist_add_string)(handle, c"mountpoint".as_ptr(), value.as_ptr()) };
        anyhow::ensure!(code == 0, "Cannot set ZFS mountpoint property: {code}");
        Ok(props)
    }
    fn create(&self, name: &str, mountpoint: Option<&Path>) -> Result<()> {
        if let Some((parent, _)) = name.rsplit_once('/') {
            self.dataset(parent)?.require_unshared()?;
        }
        let props = self.properties(mountpoint)?;
        let name_c = CString::new(name)?;
        self.check(
            unsafe {
                (self.api.zfs_create)(self.handle, name_c.as_ptr(), FILESYSTEM, props.handle)
            },
            &format!("create {name}"),
        )?;
        if mountpoint.is_some() {
            self.dataset(name)?.mount()?;
        }
        Ok(())
    }
    fn descendants(&self, root: Option<&Dataset<'_>>, snapshots: bool) -> Result<Vec<String>> {
        struct Visitor<'a> {
            connection: &'a Connection,
            names: Vec<String>,
            snapshots: bool,
            error: Option<anyhow::Error>,
        }
        unsafe extern "C" fn visit(handle: Handle, data: *mut c_void) -> c_int {
            let connection = unsafe { (*data.cast::<Visitor<'_>>()).connection };
            let snapshots = unsafe { (*data.cast::<Visitor<'_>>()).snapshots };
            let dataset = Dataset { connection, handle };
            // Never unwind across a C callback. Dataset Drop also closes handles
            // delivered by the iterator on every exit path.
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                    let name = unsafe { CStr::from_ptr((connection.api.zfs_get_name)(handle)) }
                        .to_str()?
                        .to_owned();
                    if unsafe { (connection.api.zfs_get_type)(handle) } == FILESYSTEM {
                        let iterate = if snapshots {
                            connection.api.zfs_iter_children
                        } else {
                            connection.api.zfs_iter_filesystems
                        };
                        connection.check(
                            unsafe { iterate(handle, visit, data) },
                            "iterate descendants",
                        )?;
                    }
                    unsafe { &mut *data.cast::<Visitor<'_>>() }.names.push(name);
                    Ok(())
                }));
            drop(dataset);
            let visitor = unsafe { &mut *data.cast::<Visitor<'_>>() };
            match result {
                Ok(Ok(())) => 0,
                Ok(Err(error)) => {
                    visitor.error = Some(error);
                    1
                }
                Err(_) => {
                    visitor.error = Some(anyhow!("ZFS dataset iteration panicked"));
                    1
                }
            }
        }
        let mut visitor = Visitor {
            connection: self,
            names: Vec::new(),
            snapshots,
            error: None,
        };
        let data = (&mut visitor as *mut Visitor<'_>).cast();
        let code = unsafe {
            match root {
                Some(dataset) if snapshots => {
                    (self.api.zfs_iter_children)(dataset.handle, visit, data)
                }
                Some(dataset) => (self.api.zfs_iter_filesystems)(dataset.handle, visit, data),
                None => (self.api.zfs_iter_root)(self.handle, visit, data),
            }
        };
        if let Some(error) = visitor.error {
            return Err(error);
        }
        self.check(code, "list datasets")?;
        Ok(visitor.names)
    }
    fn destroy(&self, name: &str) -> Result<()> {
        let Some(root) = self.open(name)? else {
            return Ok(());
        };
        let mut remaining = if unsafe { (self.api.zfs_get_type)(root.handle) } == FILESYSTEM {
            self.descendants(Some(&root), true)?
        } else {
            Vec::new()
        };
        remaining.push(name.to_owned());
        drop(root);
        // Clones can depend on snapshots in another child of this subtree.
        // Retry only recorded descendants; never follow external dependents.
        while !remaining.is_empty() {
            let count = remaining.len();
            let mut next = Vec::new();
            let mut failure = None;
            for candidate in remaining {
                let result = (|| {
                    if let Some(dataset) = self.open(&candidate)? {
                        if unsafe { (self.api.zfs_get_type)(dataset.handle) } == FILESYSTEM {
                            dataset.unmount()?;
                        }
                        self.check(
                            unsafe { (self.api.zfs_destroy)(dataset.handle, 0) },
                            &format!("destroy {candidate}"),
                        )?;
                    }
                    Ok::<_, anyhow::Error>(())
                })();
                if let Err(error) = result {
                    next.push(candidate);
                    failure = Some(error);
                }
            }
            if next.len() == count {
                return Err(failure.unwrap());
            }
            remaining = next;
        }
        Ok(())
    }
    fn apply(&self, mutation: DatasetMutation) -> Result<()> {
        match mutation {
            DatasetMutation::Snapshot(name) => {
                let name = CString::new(name)?;
                self.check(
                    unsafe {
                        (self.api.zfs_snapshot)(self.handle, name.as_ptr(), 0, ptr::null_mut())
                    },
                    "snapshot",
                )
            }
            DatasetMutation::Clone { snapshot, target } => {
                if let Some((parent, _)) = target.rsplit_once('/') {
                    self.dataset(parent)?.require_unshared()?;
                }
                let snapshot = self.dataset(&snapshot)?;
                let props = self.properties(None)?;
                let target = CString::new(target)?;
                self.check(
                    unsafe { (self.api.zfs_clone)(snapshot.handle, target.as_ptr(), props.handle) },
                    "clone",
                )
            }
            DatasetMutation::Rename { source, target } => {
                let dataset = self.dataset(&source)?;
                self.require_workspace_dataset(&dataset)?;
                dataset.unmount()?;
                let source = CString::new(source)?;
                let target = CString::new(target)?;
                // The kernel operation has no mount/share helper side effects.
                let code = unsafe { (self.api.lzc_rename)(source.as_ptr(), target.as_ptr()) };
                if code != 0 {
                    return Err(std::io::Error::from_raw_os_error(code).into());
                }
                Ok(())
            }
            DatasetMutation::Mount { dataset, path } => {
                self.require_workspace_dataset(&self.dataset(&dataset)?)?;
                self.dataset(&dataset)?.unmount()?;
                let properties = self.properties(path.as_deref())?;
                // ZFS_SET_NOMOUNT=1 also suppresses implicit unshare/reshare.
                self.check(
                    unsafe {
                        (self.api.zfs_prop_set_list_flags)(
                            self.dataset(&dataset)?.handle,
                            properties.handle,
                            1,
                        )
                    },
                    "set mountpoint",
                )?;
                if path.is_some() {
                    self.dataset(&dataset)?.mount()?;
                }
                Ok(())
            }
            DatasetMutation::Destroy(name) => self.destroy(&name),
        }
    }
    fn require_workspace_dataset(&self, root: &Dataset<'_>) -> Result<()> {
        root.require_unshared()?;
        anyhow::ensure!(self.descendants(Some(root), false)?.is_empty(), "Native ZFS workspace replacement requires a leaf dataset; nested filesystems are not cloned");
        Ok(())
    }
}

impl Dataset<'_> {
    fn require_unshared(&self) -> Result<()> {
        // libzfs can otherwise call the NFS/SMB sharing utilities when it
        // unmounts a filesystem. Devflow data belongs in unshared datasets.
        for property in [c"sharenfs", c"sharesmb"] {
            anyhow::ensure!(
                self.property(property)? == "off",
                "Native ZFS data operations require sharenfs=off and sharesmb=off"
            );
        }
        Ok(())
    }
    fn property(&self, name: &CStr) -> Result<String> {
        let mut value = vec![0 as c_char; 4096];
        let api = &self.connection.api;
        let property = unsafe { (api.zfs_name_to_prop)(name.as_ptr()) };
        anyhow::ensure!(property >= 0, "Unknown OpenZFS property");
        self.connection.check(
            unsafe {
                (api.zfs_prop_get)(
                    self.handle,
                    property,
                    value.as_mut_ptr(),
                    value.len(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    0,
                    1,
                )
            },
            "get property",
        )?;
        Ok(unsafe { CStr::from_ptr(value.as_ptr()) }
            .to_str()?
            .to_owned())
    }
    fn mount(&self) -> Result<()> {
        self.require_unshared()?;
        if unsafe { (self.connection.api.zfs_is_mounted)(self.handle, ptr::null_mut()) } == 0 {
            self.connection.check(
                unsafe { (self.connection.api.zfs_mount)(self.handle, ptr::null(), 0) },
                "mount",
            )?;
        }
        Ok(())
    }
    fn unmount(&self) -> Result<()> {
        self.require_unshared()?;
        #[cfg(target_os = "linux")]
        {
            let mut location = ptr::null_mut();
            let mounted =
                unsafe { (self.connection.api.zfs_is_mounted)(self.handle, &mut location) };
            if mounted == 0 {
                return Ok(());
            }
            anyhow::ensure!(
                !location.is_null(),
                "ZFS mountpoint lookup returned no path"
            );
            // zfs_is_mounted returns an allocated copy of the actual mount
            // location. Unmount directly: libzfs's wrapper can run exportfs or
            // Samba tools even when no change to sharing was requested.
            let result = nix::mount::umount(unsafe { CStr::from_ptr(location) });
            unsafe { nix::libc::free(location.cast()) };
            result.context("Cannot unmount ZFS workspace")?;
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        anyhow::bail!("Native ZFS unmount is supported on Linux only")
    }
}

fn require_native_mounts(value: Option<&std::ffi::OsStr>) -> Result<()> {
    let disabled = value.is_none_or(|value| {
        value.to_str().is_some_and(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "" | "0" | "no" | "off" | "false"
            )
        })
    });
    anyhow::ensure!(
        disabled,
        "Unset ZFS_MOUNT_HELPER (or set it to 0); devflow uses native mount operations only"
    );
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) struct NativeZfs;
async fn blocking<T: Send + 'static>(
    f: impl FnOnce(Connection) -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(move || f(Connection::new()?)).await?
}
#[async_trait::async_trait]
impl DatasetOps for NativeZfs {
    async fn exists(&self, name: &str) -> Result<bool> {
        let name = name.to_owned();
        blocking(move |zfs| Ok(zfs.open(&name)?.is_some())).await
    }
    async fn origin(&self, name: &str) -> Result<String> {
        let name = name.to_owned();
        blocking(move |zfs| zfs.dataset(&name)?.property(c"origin")).await
    }
    async fn apply(&self, mutation: DatasetMutation) -> Result<()> {
        blocking(move |zfs| zfs.apply(mutation)).await
    }
}
impl NativeZfs {
    pub async fn create(&self, name: &str, path: Option<&Path>) -> Result<()> {
        let name = name.to_owned();
        let path = path.map(Path::to_path_buf);
        blocking(move |zfs| zfs.create(&name, path.as_deref())).await
    }
    pub async fn mountpoints(&self) -> Result<Vec<(String, PathBuf)>> {
        blocking(|zfs| {
            let mut result = Vec::new();
            for name in zfs.descendants(None, false)? {
                let dataset = zfs.dataset(&name)?;
                if unsafe { (zfs.api.zfs_get_type)(dataset.handle) } == FILESYSTEM {
                    let path = dataset.property(c"mountpoint")?;
                    if Path::new(&path).is_absolute() {
                        result.push((name, PathBuf::from(path)));
                    }
                }
            }
            Ok(result)
        })
        .await
    }
    #[cfg(test)]
    pub fn destroy_sync(&self, name: &str) -> Result<()> {
        Connection::new()?.destroy(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_library_settings_that_enable_external_mount_helpers() {
        for value in [None, Some(""), Some("0"), Some("off"), Some("false")] {
            assert!(require_native_mounts(value.map(std::ffi::OsStr::new)).is_ok());
        }
        for value in ["1", "YES", "on", "0x1", " 1", "unexpected"] {
            assert!(require_native_mounts(Some(std::ffi::OsStr::new(value))).is_err());
        }
    }
}
