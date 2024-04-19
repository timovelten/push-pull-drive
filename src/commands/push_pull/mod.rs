use self::flow::PushPullFlow;
use crate::context::{Ctx, FullContext};
use clap::ValueEnum;

mod flow;
mod progress;

#[derive(clap::Args, Clone)]
pub struct PushPullArgs {
    #[arg(long)]
    pub ignore_index: bool,

    #[arg(long, value_enum, default_value_t=ConflictResolution::Interactive)]
    conflict_resolution: ConflictResolution,
}

#[derive(ValueEnum, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolution {
    Interactive,
    Normal,
    Aggressive,
    Conservative,
}

pub fn push(ctx: &impl Ctx, args: PushPullArgs) -> anyhow::Result<()> {
    let FullContext { ctx, local, remote } = ctx.get();
    let flow = PushPullFlow::new_push(local.clone(), remote.clone(), args, ctx.clone());
    flow.execute()?;

    Ok(())
}

pub fn pull(ctx: &impl Ctx, args: PushPullArgs) -> anyhow::Result<()> {
    let FullContext { ctx, local, remote } = ctx.get();
    let flow = PushPullFlow::new_pull(local.clone(), remote.clone(), args, ctx.clone());
    flow.execute()?;

    Ok(())
}
