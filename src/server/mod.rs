use async_trait::async_trait;

use crate::model::var::Objid;

pub mod parse_cmd;
pub mod scheduler;
pub mod ws_server;

#[async_trait]
pub trait Sessions: Send + Sync {
    async fn send_text(&mut self, player: Objid, msg: String) -> Result<(), anyhow::Error>;
    async fn connected_players(&mut self) -> Result<Vec<Objid>, anyhow::Error>;
}
