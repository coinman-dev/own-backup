//! Windows' own cleanup handlers — the ones Disk Cleanup (cleanmgr) runs.
//!
//! What Windows keeps for itself (the component store's superseded updates,
//! a previous installation in `Windows.old`, Delivery Optimization's cache,
//! upgrade logs) is not ours to delete file by file: ownership, hard links
//! into WinSxS and the servicing stack's own records make a plain removal
//! wrong (PLAN §6б: «штатными средствами»). Each such kind has a handler
//! registered under `HKLM\…\Explorer\VolumeCaches\<name>`, a COM object with
//! `IEmptyVolumeCache`: it says how much it would free (`GetSpaceUsed`) and
//! frees it (`Purge`). This runs the same handlers the same way, on the
//! system volume, in a single-threaded apartment of its own.

use std::sync::atomic::AtomicBool;
#[cfg(windows)]
use std::sync::{atomic::Ordering, Arc};

/// How much a handler would free (`Analyze`) or freed (`Remove`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Outcome {
    pub bytes: u64,
}

/// Run the handler registered as `name` on the volume `root` (`C:\`).
/// `remove`: purge, after measuring as Disk Cleanup does. An error says why
/// the handler could not be run (none such, refused, failed).
pub fn run(name: &str, root: &str, remove: bool, cancel: &AtomicBool) -> std::io::Result<Outcome> {
    #[cfg(windows)]
    {
        // A handler of its own thread: COM apartments are per thread, and
        // the handlers expect a single-threaded one. The callback the handler
        // holds is a COM object of its own life, so it gets a flag of its
        // own, kept in step with `cancel` while the handler works.
        let stop = Arc::new(AtomicBool::new(false));
        let done = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                while !done.load(Ordering::Relaxed) {
                    if cancel.load(Ordering::Relaxed) {
                        stop.store(true, Ordering::Relaxed);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            });
            let worker = stop.clone();
            let result = s
                .spawn(move || win::run(name, root, remove, worker))
                .join()
                .unwrap_or_else(|_| Err(std::io::Error::other("cleanup handler panicked")));
            done.store(true, Ordering::Relaxed);
            result
        })
    }
    #[cfg(not(windows))]
    {
        let _ = (name, root, remove, cancel);
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    }
}

#[cfg(windows)]
mod win {
    use std::io;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

    use windows::core::{implement, Interface, PCWSTR, PWSTR};
    use windows::Win32::Foundation::{ERROR_SUCCESS, E_ABORT};
    use windows::Win32::System::Com::{
        CLSIDFromString, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::System::Registry::{
        RegCloseKey, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
        RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
    };
    use windows::Win32::UI::LegacyWindowsEnvironmentFeatures::{
        IEmptyVolumeCache, IEmptyVolumeCache2, IEmptyVolumeCacheCallBack,
        IEmptyVolumeCacheCallBack_Impl, EMPTY_VOLUME_CACHE_FLAGS,
    };

    use super::Outcome;

    const CACHES: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\VolumeCaches";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// A handler's error as the engine reads it. «The path is not found»
    /// (`$WINDOWS.~BT`, the ESD folder) means nothing of its kind is on this
    /// system — nothing to clean, not a failure (2026-10-28, three handlers
    /// on the owner's machine).
    fn failed(e: windows::core::Error) -> io::Error {
        const FILE_NOT_FOUND: u32 = 0x8007_0002;
        const PATH_NOT_FOUND: u32 = 0x8007_0003;
        match e.code().0 as u32 {
            FILE_NOT_FOUND | PATH_NOT_FOUND => io::Error::from(io::ErrorKind::NotFound),
            _ => io::Error::other(e),
        }
    }

    /// Hears the handler's progress; answers `E_ABORT` once cancelled.
    #[implement(IEmptyVolumeCacheCallBack)]
    struct Progress {
        cancel: Arc<AtomicBool>,
        freed: Arc<AtomicU64>,
    }

    impl IEmptyVolumeCacheCallBack_Impl for Progress_Impl {
        fn ScanProgress(
            &self,
            _used: u64,
            _flags: u32,
            _status: &PCWSTR,
        ) -> windows::core::Result<()> {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(E_ABORT.into());
            }
            Ok(())
        }

        fn PurgeProgress(
            &self,
            freed: u64,
            _to_free: u64,
            _flags: u32,
            _status: &PCWSTR,
        ) -> windows::core::Result<()> {
            self.freed.store(freed, Ordering::Relaxed);
            if self.cancel.load(Ordering::Relaxed) {
                return Err(E_ABORT.into());
            }
            Ok(())
        }
    }

    /// An open registry key, closed when dropped.
    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: opened by us, closed once.
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }

    /// The COM library of this thread, released when dropped.
    struct Apartment;

    impl Drop for Apartment {
        fn drop(&mut self) {
            // SAFETY: paired with the successful CoInitializeEx below.
            unsafe { CoUninitialize() };
        }
    }

    /// A string the handler allocated, freed with the task allocator.
    fn take(p: PWSTR) {
        if !p.is_null() {
            // SAFETY: the handler gave it from CoTaskMemAlloc; freed once.
            unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
        }
    }

    fn string_value(key: HKEY, name: &str) -> Option<String> {
        let name = wide(name);
        let mut buf = vec![0u16; 512];
        let mut size = (buf.len() * 2) as u32;
        // SAFETY: the buffer and its size are live; the key is open.
        let status = unsafe {
            RegGetValueW(
                key,
                PCWSTR::null(),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        (status == ERROR_SUCCESS).then(|| {
            let n = (size as usize / 2).min(buf.len());
            String::from_utf16_lossy(&buf[..n])
                .trim_end_matches('\0')
                .to_string()
        })
    }

    fn dword_value(key: HKEY, name: &str) -> u32 {
        let name = wide(name);
        let mut v = 0u32;
        let mut size = 4u32;
        // SAFETY: a DWORD and its size; the key is open.
        let _ = unsafe {
            RegGetValueW(
                key,
                PCWSTR::null(),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_DWORD,
                None,
                Some((&mut v as *mut u32).cast()),
                Some(&mut size),
            )
        };
        v
    }

    pub(super) fn run(
        name: &str,
        root: &str,
        remove: bool,
        cancel: Arc<AtomicBool>,
    ) -> io::Result<Outcome> {
        let sub = wide(&format!(r"{CACHES}\{name}"));
        let mut hkey = HKEY::default();
        // SAFETY: the path outlives the call; `hkey` is an out-parameter.
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(sub.as_ptr()),
                0,
                KEY_READ,
                &mut hkey,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        let key = Key(hkey);
        let clsid_text = string_value(key.0, "").ok_or(io::ErrorKind::NotFound)?;
        let clsid_w = wide(&clsid_text);
        // SAFETY: a terminated string.
        let clsid =
            unsafe { CLSIDFromString(PCWSTR(clsid_w.as_ptr())) }.map_err(io::Error::other)?;
        // What Disk Cleanup passes in: the handler's own flags.
        let mut flags = EMPTY_VOLUME_CACHE_FLAGS(dword_value(key.0, "Flags"));

        // SAFETY: COM for this thread only, undone by `Apartment`.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .map_err(io::Error::other)?;
        let _apartment = Apartment;
        // SAFETY: an in-process server registered for this CLSID.
        let cache: IEmptyVolumeCache =
            unsafe { CoCreateInstance(&clsid, None, CLSCTX_INPROC_SERVER) }
                .map_err(io::Error::other)?;

        let volume = wide(root);
        let key_name = wide(name);
        let (mut shown, mut about, mut button) = (PWSTR::null(), PWSTR::null(), PWSTR::null());
        // SAFETY: out-parameters live for the call; the strings are freed
        // after. The newer interface first, as Disk Cleanup does.
        let started = unsafe {
            match cache.cast::<IEmptyVolumeCache2>() {
                Ok(two) => two.InitializeEx(
                    key.0,
                    PCWSTR(volume.as_ptr()),
                    PCWSTR(key_name.as_ptr()),
                    &mut shown,
                    &mut about,
                    &mut button,
                    &mut flags,
                ),
                Err(_) => cache.Initialize(
                    key.0,
                    PCWSTR(volume.as_ptr()),
                    &mut shown,
                    &mut about,
                    &mut flags,
                ),
            }
        };
        take(shown);
        take(about);
        take(button);
        // S_FALSE: nothing to clean on this volume.
        started.map_err(failed)?;

        let freed = Arc::new(AtomicU64::new(0));
        let progress: IEmptyVolumeCacheCallBack = Progress {
            cancel,
            freed: freed.clone(),
        }
        .into();
        let mut used = 0u64;
        // SAFETY: `used` is a live out-parameter; the callback outlives the
        // call.
        let measured = unsafe { cache.GetSpaceUsed(&mut used, &progress) };
        let outcome = match measured {
            Err(e) => Err(failed(e)),
            Ok(()) if !remove || used == 0 => Ok(Outcome { bytes: used }),
            Ok(()) => {
                // SAFETY: as above.
                match unsafe { cache.Purge(used, &progress) } {
                    Ok(()) => {
                        let told = freed.load(Ordering::Relaxed);
                        Ok(Outcome {
                            bytes: if told > 0 { told } else { used },
                        })
                    }
                    Err(e) => Err(failed(e)),
                }
            }
        };
        // SAFETY: the handler is done with; its flags are not needed.
        let _ = unsafe { cache.Deactivate() };
        outcome
    }
}
