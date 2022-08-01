use std::{future::Future, ops::Deref};

use crate::{
    handler::{ExtractError, FromRequest},
    request::WebRequest,
};

#[derive(Debug)]
pub struct PathRef<'a>(pub &'a str);

impl Deref for PathRef<'_> {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl<'a, 'r, C, B> FromRequest<'a, WebRequest<'r, C, B>> for PathRef<'a> {
    type Type<'b> = PathRef<'b>;
    type Error = ExtractError;
    type Future = impl Future<Output = Result<Self, Self::Error>> where WebRequest<'r, C, B>: 'a;

    #[inline]
    fn from_request(req: &'a WebRequest<'r, C, B>) -> Self::Future {
        async { Ok(PathRef(req.req().uri().path())) }
    }
}
