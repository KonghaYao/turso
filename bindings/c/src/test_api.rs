#[no_mangle]
pub extern "C" fn turso_test_pending_byte_get() -> u32 {
    turso_core::Database::get_pending_byte()
}

#[no_mangle]
pub extern "C" fn turso_test_pending_byte_set(value: u32) {
    turso_core::Database::set_pending_byte(value);
}
