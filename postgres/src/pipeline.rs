use core::ops::{Deref, DerefMut, Range};

use std::collections::VecDeque;

use postgres_protocol::message::{backend, frontend};
use xitca_io::bytes::BytesMut;

use super::{
    client::Client,
    column::Column,
    driver::codec::Response,
    error::Error,
    iter::{slice_iter, AsyncLendingIterator},
    row::Row,
    statement::Statement,
    BorrowToSql, ToSql,
};

/// A pipelined sql query type. It lazily batch queries into local buffer and try to send it
/// with the least amount of syscall when pipeline starts.
///
/// # Examples
/// ```rust
/// use xitca_postgres::{AsyncLendingIterator, Client, pipeline::Pipeline};
///
/// async fn pipeline(client: &Client) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
///     // prepare a statement that will be called repeatedly.
///     // it can be a collection of statements that will be called in iteration.
///     let statement = client.prepare("SELECT * FROM public.users", &[]).await?;
///
///     // create a new pipeline.
///     let mut pipe = Pipeline::new();
///
///     // pipeline can encode multiple queries.
///     pipe.query(statement.as_ref(), &[])?;
///     pipe.query_raw::<[i32; 0]>(statement.as_ref(), [])?;
///
///     // execute the pipeline and on success a streaming response will be returned.
///     let mut res = client.pipeline(pipe)?;
///
///     // iterate through the query responses. the response order is the same as the order of
///     // queries encoded into pipeline with Pipeline::query_xxx api.
///     while let Some(mut item) = res.try_next().await? {
///         // every query can contain streaming rows.
///         while let Some(row) = item.try_next().await? {
///             let _: u32 = row.get("id");
///         }
///     }
///
///     Ok(())
/// }
/// ```
pub struct Pipeline<'a, B = Owned, const SYNC_MODE: bool = true> {
    pub(crate) columns: VecDeque<&'a [Column]>,
    // type for either owned or borrowed bytes buffer.
    pub(crate) buf: B,
}

/// borrowed bytes buffer supplied by api caller
pub struct Borrowed<'a>(&'a mut BytesMut);

/// owned bytes buffer created by [Pipeline]
pub struct Owned(BytesMut);

impl Deref for Borrowed<'_> {
    type Target = BytesMut;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl DerefMut for Borrowed<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
    }
}

impl Deref for Owned {
    type Target = BytesMut;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Owned {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<'a> From<Borrowed<'a>> for Owned {
    fn from(buf: Borrowed<'a>) -> Self {
        Self(BytesMut::from(buf.as_ref()))
    }
}

impl<'a, B, const SYNC_MODE: bool> Pipeline<'a, B, SYNC_MODE>
where
    B: Into<Owned>,
{
    // partial copy where references of columns are left untouched.
    // this api is for the purpose of possible reuse/cache of pipeline buffer where encoded queries are stored.
    pub(crate) fn into_owned(self) -> Pipeline<'a, Owned, SYNC_MODE> {
        let Self { columns, buf } = self;
        Pipeline {
            columns,
            buf: buf.into(),
        }
    }
}

fn _assert_pipe_send() {
    crate::_assert_send2::<Pipeline<'_, Owned>>();
    crate::_assert_send2::<Pipeline<'_, Borrowed<'_>>>();
}

impl Default for Pipeline<'_, Owned, true> {
    fn default() -> Self {
        unimplemented!("Please use Pipeline::new or Pipeline::unsync")
    }
}

impl Pipeline<'_, Owned, true> {
    /// start a new pipeline.
    ///
    /// pipeline is sync by default. which means every query inside is considered separate binding
    /// and the pipeline is transparent to database server. the pipeline only happen on socket
    /// transport where minimal amount of syscall is needed.
    ///
    /// for more relaxed [Pipeline Mode][libpq_link] see [Pipeline::unsync] api.
    ///
    /// [libpq_link]: https://www.postgresql.org/docs/current/libpq-pipeline-mode.html
    #[inline]
    pub fn new() -> Self {
        Self::with_capacity(0)
    }
}

impl Pipeline<'_, Owned, false> {
    /// start a new un-sync pipeline.
    ///
    /// in un-sync mode pipeline treat all queries inside as one single binding and database server
    /// can see them as no sync point in between which can result in potential performance gain.
    ///
    /// it behaves the same on transportation level as [Pipeline::new] where minimal amount
    /// of socket syscall is needed.
    #[inline]
    pub fn unsync() -> Self {
        Self::with_capacity(0)
    }
}

impl<const SYNC_MODE: bool> Pipeline<'_, Owned, SYNC_MODE> {
    /// start a new pipeline with given capacity.
    /// capacity represent how many queries will be contained by a single pipeline. a determined cap
    /// can possibly reduce memory reallocation when constructing the pipeline.
    #[inline]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            columns: VecDeque::with_capacity(cap),
            buf: Owned(BytesMut::new()),
        }
    }
}

impl<'b, const SYNC_MODE: bool> Pipeline<'_, Borrowed<'b>, SYNC_MODE> {
    #[doc(hidden)]
    /// pipeline can be constructed from user supplied bytes buffer. buffer is clear when constructing a
    /// new pipeline and after [Client::pipeline] method is executed it's ownership is released.
    /// this api is for actively buffer reuse to reduce repeated heap memory allocation of executing pipeline.
    #[inline]
    pub fn with_capacity_from_buf(cap: usize, buf: &'b mut BytesMut) -> Self {
        buf.clear();
        Self {
            columns: VecDeque::with_capacity(cap),
            buf: Borrowed(buf),
        }
    }
}

impl<'a, B, const SYNC_MODE: bool> Pipeline<'a, B, SYNC_MODE>
where
    B: DerefMut<Target = BytesMut>,
{
    /// pipelined version of [Client::query] and [Client::execute]
    #[inline]
    pub fn query(&mut self, stmt: &'a Statement, params: &[&(dyn ToSql + Sync)]) -> Result<(), Error> {
        self.query_raw(stmt, slice_iter(params))
    }

    /// pipelined version of [Client::query_raw] and [Client::execute_raw]
    pub fn query_raw<I>(&mut self, stmt: &'a Statement, params: I) -> Result<(), Error>
    where
        I: IntoIterator,
        I::IntoIter: ExactSizeIterator,
        I::Item: BorrowToSql,
    {
        let params = params.into_iter();
        stmt.params_assert(&params);
        let len = self.buf.len();
        crate::query::encode::encode_maybe_sync::<_, SYNC_MODE>(&mut self.buf, stmt, params)
            .map(|_| self.columns.push_back(stmt.columns()))
            // revert back to last pipelined query when encoding error occurred.
            .inspect_err(|_| self.buf.truncate(len))
    }
}

impl Client {
    /// execute the pipeline.
    pub fn pipeline<'a, B, const SYNC_MODE: bool>(
        &self,
        mut pipe: Pipeline<'a, B, SYNC_MODE>,
    ) -> Result<PipelineStream<'a>, Error>
    where
        B: DerefMut<Target = BytesMut>,
    {
        let Pipeline { columns, ref mut buf } = pipe;
        self._pipeline::<SYNC_MODE, true>(&columns, buf)
            .map(|res| PipelineStream {
                res,
                columns,
                ranges: Vec::new(),
            })
    }

    pub(crate) fn _pipeline<const SYNC_MODE: bool, const ENCODE_SYNC: bool>(
        &self,
        columns: &VecDeque<&[Column]>,
        buf: &mut BytesMut,
    ) -> Result<Response, Error> {
        assert!(!buf.is_empty());

        let sync_count = if SYNC_MODE {
            columns.len()
        } else {
            if ENCODE_SYNC {
                frontend::sync(buf);
            }
            1
        };

        self.tx.send_multi_with(
            |b| {
                b.extend_from_slice(buf);
                Ok(())
            },
            sync_count,
        )
    }
}

/// streaming response of pipeline.
/// impl [AsyncLendingIterator] trait and can be collected asynchronously.
pub struct PipelineStream<'a> {
    pub(crate) res: Response,
    pub(crate) columns: VecDeque<&'a [Column]>,
    pub(crate) ranges: Vec<Option<Range<usize>>>,
}

impl<'a> AsyncLendingIterator for PipelineStream<'a> {
    type Ok<'i> = PipelineItem<'i, 'a> where Self: 'i;
    type Err = Error;

    async fn try_next(&mut self) -> Result<Option<Self::Ok<'_>>, Self::Err> {
        while !self.columns.is_empty() {
            match self.res.recv().await? {
                backend::Message::BindComplete => {
                    let columns = self
                        .columns
                        .pop_front()
                        .expect("PipelineItem must not overflow PipelineStream's columns array");
                    return Ok(Some(PipelineItem {
                        finished: false,
                        stream: self,
                        columns,
                    }));
                }
                backend::Message::DataRow(_) | backend::Message::CommandComplete(_) => {
                    // last PipelineItem dropped before finish. do some catch up until next
                    // item arrives.
                }
                backend::Message::ReadyForQuery(_) => {}
                _ => return Err(Error::unexpected()),
            }
        }

        Ok(None)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.columns.len();
        (len, Some(len))
    }
}

/// streaming item of certain query inside pipeline's [PipelineStream].
/// impl [AsyncLendingIterator] and can be used to collect [Row] from item.
pub struct PipelineItem<'a, 'c> {
    finished: bool,
    stream: &'a mut PipelineStream<'c>,
    columns: &'a [Column],
}

impl PipelineItem<'_, '_> {
    /// collect rows affected by this pipelined query. [Row] information will be ignored.
    ///
    /// # Panic
    /// calling this method on an already finished PipelineItem will cause panic. PipelineItem is marked as finished
    /// when its [AsyncLendingIterator::try_next] method returns [Option::None]
    pub async fn row_affected(mut self) -> Result<u64, Error> {
        assert!(!self.finished, "PipelineItem has already finished");
        loop {
            match self.stream.res.recv().await? {
                backend::Message::DataRow(_) => {}
                backend::Message::CommandComplete(body) => {
                    self.finished = true;
                    return crate::query::decode::body_to_affected_rows(&body);
                }
                _ => return Err(Error::unexpected()),
            }
        }
    }
}

impl AsyncLendingIterator for PipelineItem<'_, '_> {
    type Ok<'i> = Row<'i> where Self: 'i;
    type Err = Error;

    async fn try_next(&mut self) -> Result<Option<Self::Ok<'_>>, Self::Err> {
        if !self.finished {
            match self.stream.res.recv().await? {
                backend::Message::DataRow(body) => {
                    return Row::try_new(self.columns, body, &mut self.stream.ranges).map(Some);
                }
                backend::Message::CommandComplete(_) => self.finished = true,
                _ => return Err(Error::unexpected()),
            }
        }

        Ok(None)
    }
}
