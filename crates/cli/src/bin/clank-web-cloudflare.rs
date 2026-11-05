fn main() {
    clank_web_cloudflare::process::main(
        std::sync::Arc::new(clank::proc_run::TunnelSpawn),
        clank::cli::team::check_user_config,
    );
}
