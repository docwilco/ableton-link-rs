// Platform-specific thread factory implementations
// Based on Ableton Link's thread optimizations

use std::thread::{self, JoinHandle};

pub struct ThreadFactory;

impl ThreadFactory {
    /// Create a new thread with platform-specific optimizations
    /// Sets thread name for debugging purposes
    pub fn make_thread<F, T>(name: String, f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            thread::Builder::new()
                .name(name.clone())
                .spawn(move || {
                    // Set thread name using pthread API for better debugging
                    Self::set_thread_name(&name);
                    f()
                })
                .expect("Failed to spawn thread")
        }

        #[cfg(target_os = "windows")]
        {
            thread::Builder::new()
                .name(name.clone())
                .spawn(move || {
                    // Set thread description on Windows
                    Self::set_windows_thread_description(&name);
                    f()
                })
                .expect("Failed to spawn thread")
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            thread::Builder::new()
                .name(name)
                .spawn(f)
                .expect("Failed to spawn thread")
        }
    }

    #[cfg(target_os = "macos")]
    fn set_thread_name(name: &str) {
        use std::ffi::CString;
        
        if let Ok(c_name) = CString::new(name) {
            unsafe {
                // macOS uses pthread_setname_np with just the name
                libc::pthread_setname_np(c_name.as_ptr());
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn set_thread_name(name: &str) {
        use std::ffi::CString;
        
        if let Ok(c_name) = CString::new(name) {
            unsafe {
                // Linux uses pthread_setname_np with thread handle and name
                libc::pthread_setname_np(libc::pthread_self(), c_name.as_ptr());
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn set_windows_thread_description(name: &str) {
        // Convert to wide string for Windows
        let wide_name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        
        unsafe {
            // Try to use SetThreadDescription if available (Windows 10 version 1607+)
            let kernel32 = winapi::um::libloaderapi::GetModuleHandleA(
                b"kernel32.dll\0".as_ptr() as *const i8
            );
            
            if !kernel32.is_null() {
                let set_thread_desc = winapi::um::libloaderapi::GetProcAddress(
                    kernel32,
                    b"SetThreadDescription\0".as_ptr() as *const i8
                );
                
                if !set_thread_desc.is_null() {
                    let func: extern "system" fn(winapi::shared::ntdef::HANDLE, *const u16) -> winapi::shared::minwindef::HRESULT = 
                        std::mem::transmute(set_thread_desc);
                    func(winapi::um::processthreadsapi::GetCurrentThread(), wide_name.as_ptr());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn test_thread_creation() {
        let result = Arc::new(Mutex::new(None));
        let result_clone = result.clone();

        let handle = ThreadFactory::make_thread("test_thread".to_string(), move || {
            *result_clone.lock().unwrap() = Some(42);
            42
        });

        let thread_result = handle.join().unwrap();
        assert_eq!(thread_result, 42);
        assert_eq!(*result.lock().unwrap(), Some(42));
    }

    #[test]
    fn test_thread_naming() {
        let handle = ThreadFactory::make_thread("named_thread".to_string(), || {
            // Just verify the thread was created successfully
            std::thread::current().name().map(|s| s.to_string())
        });

        let name = handle.join().unwrap();
        // Note: thread name might be truncated on some platforms
        if let Some(actual_name) = name {
            assert!(!actual_name.is_empty());
        }
    }
}
