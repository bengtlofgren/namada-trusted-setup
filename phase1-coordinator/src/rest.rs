//! REST API endpoints.

use crate::{
    objects::Task,
    storage::{ContributionLocator, ContributionSignatureLocator},
    ContributionFileSignature,
};
use rocket::{
    error,
    get,
    http::{ContentType, Status},
    post,
    response::{Responder, Response},
    serde::{json::Json, Deserialize, Serialize},
    Request,
    State,
};

use crate::{objects::LockedLocators, CoordinatorError, Participant};

use std::{collections::LinkedList, io::Cursor, net::SocketAddr, sync::Arc};
use thiserror::Error;

use tokio::sync::RwLock;

type Coordinator = Arc<RwLock<crate::Coordinator>>;

#[derive(Error, Debug)]
pub enum ResponseError {
    #[error("Coordinator failed: {0}")]
    CoordinatorError(CoordinatorError),
    #[error("Could not find contributor with public key {0}")]
    UnknownContributor(String),
    #[error("Could not find the provided Task {0} in coordinator state")]
    UnknownTask(Task),
}

impl<'r> Responder<'r, 'static> for ResponseError {
    fn respond_to(self, _request: &'r Request<'_>) -> rocket::response::Result<'static> {
        let response = format!("{}", self);
        Response::build()
            .status(Status::InternalServerError)
            .header(ContentType::JSON)
            .sized_body(response.len(), Cursor::new(response))
            .ok()
    }
}

type Result<T> = std::result::Result<T, ResponseError>;

#[derive(Deserialize, Serialize)]
pub struct ChunkRequest {
    pub pubkey: String,
    pub locked_locators: LockedLocators,
}

#[derive(Deserialize, Serialize)]
pub struct ContributeChunkRequest {
    pub pubkey: String,
    pub chunk_id: u64,
}

#[derive(Deserialize, Serialize)]
pub struct PostChunkRequest {
    pub contribution_locator: ContributionLocator,
    pub contribution: Vec<u8>,
    pub contribution_file_signature_locator: ContributionSignatureLocator,
    pub contribution_file_signature: ContributionFileSignature,
}

//
// -- REST API ENDPOINTS --
//

/// Add the incoming [`Contributor`] to the queue of contributors.
#[post("/contributor/join_queue", format = "json", data = "<contributor_public_key>")]
pub async fn join_queue(
    coordinator: &State<Coordinator>,
    contributor_public_key: Json<String>,
    contributor_ip: SocketAddr,
) -> Result<()> {
    let pubkey = contributor_public_key.into_inner();

    // Add new contributor to queue
    let contributor = Participant::new_contributor(pubkey.as_str());

    match coordinator
        .write()
        .await
        .add_to_queue(contributor, Some(contributor_ip.ip()), 10)
    {
        Ok(()) => Ok(()),
        Err(e) => Err(ResponseError::CoordinatorError(e)),
    }
}

/// Lock a chunk in the ceremony. This should be the first function called when attempting to contribute to a chunk. Once the chunk is locked, it is ready to be downloaded.
#[post("/contributor/lock_chunk", format = "json", data = "<contributor_public_key>")]
pub async fn lock_chunk(
    coordinator: &State<Coordinator>,
    contributor_public_key: Json<String>,
) -> Result<Json<LockedLocators>> {
    let pubkey = contributor_public_key.into_inner();
    let contributor = Participant::new_contributor(pubkey.as_str());

    match coordinator.write().await.try_lock(&contributor) {
        Ok((_, locked_locators)) => Ok(Json(locked_locators)),
        Err(e) => Err(ResponseError::CoordinatorError(e)),
    }
}

/// Download a chunk from the coordinator, which should be contributed to upon receipt.
#[get("/download/chunk", format = "json", data = "<get_chunk_request>")]
pub async fn get_chunk(coordinator: &State<Coordinator>, get_chunk_request: Json<ChunkRequest>) -> Result<Json<Task>> {
    let request = get_chunk_request.into_inner();
    let contributor = Participant::new_contributor(request.pubkey.as_str());

    let next_contribution = request.locked_locators.next_contribution();

    // Build and check next Task
    let task = Task::new(next_contribution.chunk_id(), next_contribution.contribution_id());

    match coordinator.read().await.state().current_participant_info(&contributor) {
        Some(info) => {
            if !info.pending_tasks().contains(&task) {
                return Err(ResponseError::UnknownTask(task));
            }
            Ok(Json(task))
        }
        None => Err(ResponseError::UnknownContributor(request.pubkey)),
    }
}

/// Upload a chunk contribution to the coordinator. Write the contribution bytes to
/// disk at the provided [`Locator`]. Also writes the corresponding [`ContributionFileSignature`]
#[post("/upload/chunk", format = "json", data = "<post_chunk_request>")]
pub async fn post_contribution_chunk(
    coordinator: &State<Coordinator>,
    post_chunk_request: Json<PostChunkRequest>,
) -> Result<()> {
    let request = post_chunk_request.into_inner();

    if let Err(e) = coordinator
        .write()
        .await
        .write_contribution(request.contribution_locator, request.contribution)
    {
        return Err(ResponseError::CoordinatorError(e));
    }

    match coordinator.write().await.write_contribution_file_signature(
        request.contribution_file_signature_locator,
        request.contribution_file_signature,
    ) {
        Ok(()) => Ok(()),
        Err(e) => Err(ResponseError::CoordinatorError(e)),
    }
}

/// Notify the coordinator of a finished and uploaded contribution. This will unlock the given chunk and allow the contributor to take on a new task.
#[post(
    "/contributor/contribute_chunk",
    format = "json",
    data = "<contribute_chunk_request>"
)]
pub async fn contribute_chunk(
    coordinator: &State<Coordinator>,
    contribute_chunk_request: Json<ContributeChunkRequest>,
) -> Result<Json<ContributionLocator>> {
    let request = contribute_chunk_request.into_inner();
    let contributor = Participant::new_contributor(request.pubkey.as_str());

    match coordinator.write().await.try_contribute(&contributor, request.chunk_id) {
        Ok(contribution_locator) => Ok(Json(contribution_locator)),
        Err(e) => Err(ResponseError::CoordinatorError(e)),
    }
}

/// Update the coordinator state.
#[get("/update")]
pub async fn update_coordinator(coordinator: &State<Coordinator>) -> Result<()> {
    match coordinator.write().await.update() {
        Ok(()) => Ok(()),
        Err(e) => Err(ResponseError::CoordinatorError(e)),
    }
}

/// Lets the coordinator know that the participant is still alive and participating (or waiting to participate) in the ceremony.
#[post("/contributor/heartbeat", format = "json", data = "<contributor_public_key>")]
pub async fn heartbeat(coordinator: &State<Coordinator>, contributor_public_key: Json<String>) -> Result<()> {
    let pubkey = contributor_public_key.into_inner();
    let contributor = Participant::new_contributor(pubkey.as_str());
    match coordinator.write().await.heartbeat(&contributor) {
        Ok(()) => Ok(()),
        Err(e) => Err(ResponseError::CoordinatorError(e)),
    }
}

/// Get the pending tasks of contributor.
#[get("/contributor/get_tasks_left", format = "json", data = "<contributor_public_key>")]
pub async fn get_tasks_left(
    coordinator: &State<Coordinator>,
    contributor_public_key: Json<String>,
) -> Result<Json<LinkedList<Task>>> {
    let pubkey = contributor_public_key.into_inner();
    let contributor = Participant::new_contributor(pubkey.as_str());

    match coordinator.read().await.state().current_participant_info(&contributor) {
        Some(info) => Ok(Json(info.pending_tasks().to_owned())),
        None => Err(ResponseError::UnknownContributor(pubkey)),
    }
}