//! stakes generator

#[derive(Debug)]
pub struct StakerInfo {
    pub name: &'static str,
    pub staker: &'static str,
    pub withdrawer: Option<&'static str>,
    pub lamports: u64,
}
