fn main() -> std::io::Result<()> {
    zcutils::crypto_policy::initialize_or_exit();
    zcutils::wal_failover::main_entry()
}
