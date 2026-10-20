use anyhow::Result;

fn main() -> Result<()> {
    // Env parity with scripts/run-goose.sh before goose's config is touched.
    goose_readline::agent::env_setup::apply_defaults();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(goose_readline::app::main_main())
}
