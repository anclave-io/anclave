fn main() {
    let log = anclave_audit::AuditLog::new(std::env::args().nth(1).unwrap());
    println!("  {:?}", log.verify().unwrap());
}
