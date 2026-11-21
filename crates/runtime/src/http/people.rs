//! People and their keys: the owner adds people and makes their keys, and
//! anyone may see who they are and list or revoke their own keys.

use axum::Extension;
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use schema::schema_of;
use store::{PageRequest, RecordId};

use super::rest::{Params, only, page};
use super::spec::{Body, Kind, LIMIT, OFFSET, Param, Route};
use super::{ApiError, App, json_body};
use crate::auth::{
    self, Caller, CallerView, KeyList, KeyMade, KeyRequest, PERSON_PATTERN, PersonList,
    PersonRequest, PersonView, SLUG_PATTERN,
};

/// A key's id from the path.
fn key_id(id: &str) -> Result<RecordId, ApiError> {
    uuid::Uuid::try_parse(id)
        .map(RecordId::from_uuid)
        .map_err(|_| ApiError::invalid("a key's id is a UUID"))
}

async fn ready(app: &App) -> Result<(), ApiError> {
    app.auth_ready()
        .await
        .map_err(|error| ApiError::internal(&error))
}

async fn put_person(
    State(app): State<App>,
    Extension(caller): Extension<Caller>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    if !auth::valid_slug(&slug) {
        return Err(ApiError::invalid(
            "a slug is 1 to 48 of a-z, 0-9 and -, not starting with -",
        ));
    }
    let request: PersonRequest = app
        .person_request
        .parse(json_body(&headers, &body)?)
        .map_err(|refusal| ApiError::refused(&refusal))?;
    ready(&app).await?;
    let store = app.runtime.workflows.store.as_ref();
    let (person, is_new) = auth::put_person(store, &slug, request, &caller.actor)
        .await
        .map_err(|error| ApiError::internal(&error))?;
    let status = if is_new {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(person)).into_response())
}

async fn list_people(
    State(app): State<App>,
    Query(params): Params,
) -> Result<Json<PersonList>, ApiError> {
    only(&params, &["limit", "offset"])?;
    let page = page(&params)?;
    ready(&app).await?;
    let found = auth::people(app.runtime.workflows.store.as_ref(), page)
        .await
        .map_err(|error| ApiError::internal(&error))?;
    Ok(Json(PersonList {
        items: found.items,
        has_more: found.has_more,
    }))
}

async fn make_key(
    State(app): State<App>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let request: KeyRequest = app
        .key_request
        .parse(json_body(&headers, &body)?)
        .map_err(|refusal| ApiError::refused(&refusal))?;
    ready(&app).await?;
    let store = app.runtime.workflows.store.as_ref();
    let person = auth::person(store, &request.person)
        .await
        .map_err(|error| ApiError::internal(&error))?
        .ok_or_else(|| ApiError::not_found("there's no person with that id"))?;
    if let Some(scope) = request
        .scopes
        .iter()
        .find(|scope| !person.role.allows(**scope))
    {
        return Err(ApiError::forbidden(&format!(
            "{}'s role doesn't allow the {} scope",
            person.id,
            scope.name()
        )));
    }
    let made: KeyMade = auth::make_key(store, request, &caller.actor)
        .await
        .map_err(|error| ApiError::internal(&error))?;
    Ok((StatusCode::CREATED, Json(made)).into_response())
}

async fn listed_keys(
    app: &App,
    person: Option<&str>,
    page: PageRequest,
) -> Result<Json<KeyList>, ApiError> {
    ready(app).await?;
    let found = auth::keys(app.runtime.workflows.store.as_ref(), person, page)
        .await
        .map_err(|error| ApiError::internal(&error))?;
    Ok(Json(KeyList {
        items: found.items,
        has_more: found.has_more,
    }))
}

async fn list_keys(
    State(app): State<App>,
    Query(params): Params,
) -> Result<Json<KeyList>, ApiError> {
    only(&params, &["person", "limit", "offset"])?;
    let page = page(&params)?;
    let person = params.get("person").map(String::as_str);
    if person.is_some_and(|person| !auth::valid_person(person)) {
        return Err(ApiError::invalid("person is `person:` and a slug"));
    }
    listed_keys(&app, person, page).await
}

async fn revoked(
    app: &App,
    id: &str,
    person: Option<&str>,
    caller: &Caller,
) -> Result<StatusCode, ApiError> {
    let id = key_id(id)?;
    ready(app).await?;
    let found = auth::revoke(
        app.runtime.workflows.store.as_ref(),
        id,
        person,
        &caller.actor,
    )
    .await
    .map_err(|error| ApiError::internal(&error))?;
    if found {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("there's no such key"))
    }
}

async fn revoke_key(
    State(app): State<App>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    revoked(&app, &id, None, &caller).await
}

async fn me(
    State(app): State<App>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<CallerView>, ApiError> {
    let mut view = caller.view();
    if let Some(payer) = app.runtime.payer_for(Some(caller.actor.as_str())).await {
        let store = app.runtime.workflows.store.as_ref();
        view.spend = Some(
            auth::spend_today(store, payer)
                .await
                .map_err(|error| ApiError::internal(&error))?,
        );
    }
    Ok(Json(view))
}

async fn list_my_keys(
    State(app): State<App>,
    Extension(caller): Extension<Caller>,
    Query(params): Params,
) -> Result<Json<KeyList>, ApiError> {
    only(&params, &["limit", "offset"])?;
    let page = page(&params)?;
    if caller.is_owner() {
        // The owner key isn't a record, and the owner has no others.
        return Ok(Json(KeyList {
            items: Vec::new(),
            has_more: false,
        }));
    }
    listed_keys(&app, Some(caller.actor.as_str()), page).await
}

async fn revoke_my_key(
    State(app): State<App>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if caller.is_owner() {
        return Err(ApiError::not_found(
            "the owner has no keys of its own; DELETE /api/v1/keys/{id} revokes anyone's",
        ));
    }
    let person = caller.actor.to_string();
    revoked(&app, &id, Some(&person), &caller).await
}

const SLUG: Param = Param {
    name: "slug",
    about: "The person's slug: their id without `person:`.",
    kind: Kind::Matching(SLUG_PATTERN),
};
const KEY_ID: Param = Param {
    name: "id",
    about: "The key's id, a UUID.",
    kind: Kind::Uuid,
};
const PERSON: Param = Param {
    name: "person",
    about: "Only this person's keys: `person:` and their slug.",
    kind: Kind::Matching(PERSON_PATTERN),
};

pub(super) fn routes() -> Vec<Route> {
    vec![
        Route::put(
            "put_person",
            "/api/v1/people/{slug}",
            "Add a person, or change one's name, role or whether they're disabled",
            put(put_person),
        )
        .path(&[SLUG])
        .body(Body::Typed(schema_of::<PersonRequest>))
        .reply(
            201,
            "They were added.",
            Body::Typed(schema_of::<PersonView>),
        )
        .reply(
            200,
            "They were changed.",
            Body::Typed(schema_of::<PersonView>),
        ),
        Route::get(
            "list_people",
            "/api/v1/people",
            "The people",
            get(list_people),
        )
        .query(&[LIMIT, OFFSET])
        .reply(
            200,
            "A page of people.",
            Body::Typed(schema_of::<PersonList>),
        ),
        Route::post(
            "make_key",
            "/api/v1/keys",
            "Make a key for a person. Its secret is shown this once",
            post(make_key),
        )
        .body(Body::Typed(schema_of::<KeyRequest>))
        .reply(
            201,
            "The key, with its secret.",
            Body::Typed(schema_of::<KeyMade>),
        ),
        Route::get(
            "list_keys",
            "/api/v1/keys",
            "The keys, without their secrets",
            get(list_keys),
        )
        .query(&[PERSON, LIMIT, OFFSET])
        .reply(200, "A page of keys.", Body::Typed(schema_of::<KeyList>)),
        Route::delete(
            "revoke_key",
            "/api/v1/keys/{id}",
            "Revoke a key. It stops working at once",
            delete(revoke_key),
        )
        .path(&[KEY_ID])
        .reply(204, "It's revoked.", Body::Empty),
        Route::get(
            "me",
            "/api/v1/me",
            "Who this key is, and what it may do",
            get(me),
        )
        .reply(200, "The caller.", Body::Typed(schema_of::<CallerView>)),
        Route::get(
            "list_my_keys",
            "/api/v1/me/keys",
            "This key's person's keys",
            get(list_my_keys),
        )
        .query(&[LIMIT, OFFSET])
        .reply(200, "A page of keys.", Body::Typed(schema_of::<KeyList>)),
        Route::delete(
            "revoke_my_key",
            "/api/v1/me/keys/{id}",
            "Revoke one of this key's person's keys",
            delete(revoke_my_key),
        )
        .path(&[KEY_ID])
        .reply(204, "It's revoked.", Body::Empty),
    ]
}
