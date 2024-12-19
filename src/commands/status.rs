use crate::{
    context::Ctx,
    file_repository::FileRepository,
};

#[derive(clap::Args)]
pub struct StatusArgs;

pub fn local_status<R: FileRepository>(_ctx: &Ctx<R>, _args: StatusArgs) -> anyhow::Result<()> {
    todo!()
}

pub fn remote_status<R: FileRepository>(_ctx: &Ctx<R>, _args: StatusArgs) -> anyhow::Result<()> {
    todo!()
}
