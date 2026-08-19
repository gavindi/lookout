use bytes::BytesMut;
use imap_proto::{RequestId, Response};

/// An owned, parsed server response.
///
/// Parsing borrows from the buffer it was read out of (`Response<'a>`'s fields
/// are mostly `Cow<'a, _>`), but this type outlives that buffer - the stream
/// reuses/advances its buffer on the very next read. So construction always
/// converts to `Response<'static>` up front via `into_owned()` rather than
/// keeping the input buffer alive alongside a borrow into it.
#[derive(Debug, PartialEq, Eq)]
pub struct ResponseData(Response<'static>);

impl ResponseData {
    /// Parses a response out of `owner` and stores it as an owned value.
    ///
    /// `owner` only needs to live for the duration of `f` - once `f` returns,
    /// nothing further borrows from it.
    pub fn try_new<Err>(
        owner: BytesMut,
        f: impl for<'a> FnOnce(&'a BytesMut) -> Result<Response<'a>, Err>,
    ) -> Result<Self, Err> {
        let response = f(&owner)?;
        Ok(ResponseData(response.into_owned()))
    }

    /// Wraps an already-owned response, with no buffer of its own.
    pub(crate) fn from_owned(response: Response<'static>) -> Self {
        ResponseData(response)
    }

    pub fn request_id(&self) -> Option<&RequestId> {
        match &self.0 {
            Response::Done { ref tag, .. } => Some(tag),
            _ => None,
        }
    }

    pub fn parsed(&self) -> &Response<'_> {
        &self.0
    }
}
