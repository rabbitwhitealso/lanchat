//! 系统随机数（只用标准库 + 操作系统自带接口，不引入 rand/getrandom）

use std::io;

#[cfg(windows)]
pub fn fill(buf: &mut [u8]) -> io::Result<()> {
    #[link(name = "bcrypt")]
    extern "system" {
        fn BCryptGenRandom(
            h_algorithm: *mut core::ffi::c_void,
            pb_buffer: *mut u8,
            cb_buffer: u32,
            dw_flags: u32,
        ) -> i32;
    }
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Other,
            format!("BCryptGenRandom 失败: {status:#x}"),
        ))
    }
}

#[cfg(not(windows))]
pub fn fill(buf: &mut [u8]) -> io::Result<()> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(buf)
}

#[cfg(test)]
mod tests {
    #[test]
    fn fills_and_differs() {
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        super::fill(&mut a).unwrap();
        super::fill(&mut b).unwrap();
        assert_ne!(a, b);
        assert_ne!(a, [0u8; 32]);
    }
}
