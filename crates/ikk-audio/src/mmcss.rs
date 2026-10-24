//! Windows MMCSS: put the audio callback thread into the "Pro Audio"
//! scheduler class to reduce jitter. Linked directly against avrt.dll.
//! No-op on other platforms (CoreAudio and AAudio are realtime already).

mod imp {
    use std::cell::Cell;
    use std::ffi::c_void;

    #[link(name = "avrt")]
    extern "system" {
        fn AvSetMmThreadCharacteristicsW(task_name: *const u16, task_index: *mut u32) -> *mut c_void;
    }

    thread_local! {
        static HANDLE: Cell<*mut c_void> = const { Cell::new(std::ptr::null_mut()) };
    }

    pub fn elevate_current_thread_once() {
        HANDLE.with(|cell| {
            if !cell.get().is_null() {
                return;
            }
            let name: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
            let mut idx: u32 = 0;
            let h = unsafe { AvSetMmThreadCharacteristicsW(name.as_ptr(), &mut idx) };
            if h.is_null() {
                tracing::warn!("MMCSS elevation failed; audio runs at normal priority");
            } else {
                cell.set(h);
            }
        });
    }
}

/// Idempotent per thread; safe to call from the capture callback.
#[inline]
pub fn elevate_audio_thread() {
    imp::elevate_current_thread_once();
}
