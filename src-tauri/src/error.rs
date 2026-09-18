pub type CommandResult<T> = Result<T, String>;

pub fn command<T>(result: anyhow::Result<T>) -> CommandResult<T> {
    result.map_err(|error| format!("{error:#}"))
}
