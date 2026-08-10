use axum::{
	async_trait,
	body::Bytes,
	extract::{FromRequest, Request},
	http::{header, HeaderValue, StatusCode},
	response::{IntoResponse, Response},
};
use serde::{de::DeserializeOwned, Serialize};

/// MessagePack extractor and responder replacing the `actix-msgpack` crate
/// from the Argon fork base, preserving its exact wire behavior: requests
/// must carry the `application/msgpack` content type and be non-empty
/// (`400 Bad Request` otherwise, `413 Payload Too Large` above the body
/// limit), while responses are encoded with `rmp_serde::to_vec_named`
pub struct MsgPack<T>(pub T);

#[async_trait]
impl<S, T> FromRequest<S> for MsgPack<T>
where
	T: DeserializeOwned,
	S: Send + Sync,
{
	type Rejection = Response;

	async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
		if !has_msgpack_content_type(&request) {
			return Err(StatusCode::BAD_REQUEST.into_response());
		}

		let bytes = Bytes::from_request(request, state)
			.await
			.map_err(IntoResponse::into_response)?;

		if bytes.is_empty() {
			return Err(StatusCode::BAD_REQUEST.into_response());
		}

		match rmp_serde::from_slice(&bytes) {
			Ok(value) => Ok(Self(value)),
			Err(_) => Err(StatusCode::BAD_REQUEST.into_response()),
		}
	}
}

impl<T: Serialize> IntoResponse for MsgPack<T> {
	fn into_response(self) -> Response {
		match rmp_serde::to_vec_named(&self.0) {
			Ok(body) => (
				[(header::CONTENT_TYPE, HeaderValue::from_static("application/msgpack"))],
				body,
			)
				.into_response(),
			Err(_) => StatusCode::BAD_REQUEST.into_response(),
		}
	}
}

impl<T> std::ops::Deref for MsgPack<T> {
	type Target = T;

	fn deref(&self) -> &T {
		&self.0
	}
}

fn has_msgpack_content_type(request: &Request) -> bool {
	request
		.headers()
		.get(header::CONTENT_TYPE)
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.parse::<mime::Mime>().ok())
		.is_some_and(|mime| mime.type_() == mime::APPLICATION && mime.subtype() == "msgpack")
}
