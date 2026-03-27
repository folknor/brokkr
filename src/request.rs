/// Query parameters for the results command.
pub(crate) struct ResultsQuery {
    pub(crate) query: Option<String>,
    pub(crate) commit: Option<String>,
    pub(crate) compare: Option<Vec<String>>,
    pub(crate) compare_last: bool,
    pub(crate) command: Option<String>,
    pub(crate) variant: Option<String>,
    pub(crate) limit: usize,
    pub(crate) top: usize,
}
