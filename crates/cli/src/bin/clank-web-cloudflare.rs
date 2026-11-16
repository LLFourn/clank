fn main() {
    clank_web_cloudflare::process::main(
        std::sync::Arc::new(clank::proc_run::TunnelSpawn),
        std::sync::Arc::new(clank::store_watch::KernelStoreWatch),
        clank::cli::team::check_user_config,
    );
}
