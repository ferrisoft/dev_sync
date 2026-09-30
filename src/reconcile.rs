//! Making the disk match the layout: plan the changes, then carry them out (§8.7, §8.8).

mod execute;
mod facts;
mod plan;

pub(crate) use execute::describe_unknown_parked;
pub(crate) use execute::execute;
pub(crate) use execute::restore_interrupted_moves;
pub(crate) use facts::collect_facts;
pub(crate) use plan::landing;
pub(crate) use plan::plan;
