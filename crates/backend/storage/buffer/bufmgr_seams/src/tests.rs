use super::*;

#[test]
fn active_buffers_follow_the_published_pool_else_boot() {
    assert_eq!(active_buffers_or(1024), 1024);
    publish_active_buffers(4096);
    assert_eq!(active_buffers_or(1024), 4096);
    publish_active_buffers(2560);
    assert_eq!(active_buffers_or(1024), 2560);
    publish_active_buffers(0);
    assert_eq!(active_buffers_or(1024), 1024);
}
