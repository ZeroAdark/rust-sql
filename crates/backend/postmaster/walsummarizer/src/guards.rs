pub(super) struct WalSegmentGuard(pub i32);

impl WalSegmentGuard {
    pub fn close(&mut self) {
        if self.0 >= 0 {
            fd::pg_close(std::mem::replace(&mut self.0, -1));
        }
    }
}

impl Drop for WalSegmentGuard {
    fn drop(&mut self) {
        self.close();
    }
}
