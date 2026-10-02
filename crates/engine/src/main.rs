fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    // A malformed command line is an error before any engine state exists: no window is opened for
    // a run that was asked for a flag's value and did not get one.
    let config =
        engine::config::EngineConfig::from_env().map_err(|error| color_eyre::eyre::eyre!(error))?;
    engine::run(config)
}
