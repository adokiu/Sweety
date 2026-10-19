use core::{
    pin::Pin,
    task::{Context, Poll},
};

use http_body_alt::{Body, Frame, SizeHint};
use pin_project_lite::pin_project;

use sweety_http_core::body::NONE_BODY_HINT;

use super::BodyStream;

pin_project! {
    /// [`BodyStream`] → [`http_body_alt::Body`] for http-encoding 0.3.
    #[derive(Debug)]
    pub struct StreamAsBody<B> {
        #[pin]
        inner: B,
    }
}

impl<B> StreamAsBody<B> {
    #[inline]
    pub fn new(inner: B) -> Self {
        Self { inner }
    }

    #[inline]
    pub fn into_inner(self) -> B {
        self.inner
    }
}

impl<B> Body for StreamAsBody<B>
where
    B: BodyStream,
{
    type Data = B::Chunk;
    type Error = B::Error;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.project().inner.poll_next(cx).map_ok(Frame::Data)
    }

    #[inline]
    fn is_end_stream(&self) -> bool {
        false
    }

    #[inline]
    fn size_hint(&self) -> SizeHint {
        match self.inner.size_hint() {
            (low, Some(up)) if low == up => SizeHint::Exact(u64::try_from(low).unwrap_or(u64::MAX)),
            NONE_BODY_HINT => SizeHint::None,
            _ => SizeHint::Unknown,
        }
    }
}
