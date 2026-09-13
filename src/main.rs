fn main() -> std::io::Result<()> {
    zcutils::fips_application_checks::emit_if_requested();
    zcutils::crypto_policy::initialize_or_exit();
    zcutils::main_entry()
}
