use std::pin::Pin;

use arrow_flight::{
    encode::FlightDataEncoderBuilder, error::FlightError,
    flight_service_server::FlightService as Flight, Action, ActionType, Criteria, Empty,
    FlightData, FlightDescriptor, FlightInfo, HandshakeRequest, HandshakeResponse, IpcMessage,
    PutResult, SchemaResult, Ticket,
};
use data_types::{NamespaceId, PartitionId, TableId};
use flatbuffers::FlatBufferBuilder;
use futures::{Stream, StreamExt, TryStreamExt};
use ingester_query_grpc::influxdata::iox::ingester::v1 as proto;
use metric::U64Counter;
use observability_deps::tracing::*;
use prost::Message;
use thiserror::Error;
use tokio::sync::{Semaphore, TryAcquireError};
use tonic::{Request, Response, Streaming};
use trace::{ctx::SpanContext, span::SpanExt};

use crate::{
    ingester_id::IngesterId,
    query::{response::QueryResponse, QueryError, QueryExec},
};

/// Error states for the query RPC handler.
///
/// Note that this DOES NOT include any query-time error states - those are
/// mapped directly from the [`QueryError`] itself.
///
/// Note that this isn't strictly necessary as the [`FlightService`] trait
/// expects a [`tonic::Status`] error value, but by defining the errors here
/// they serve as documentation of the potential error states (which are then
/// converted into [`tonic::Status`] for the handler).
#[derive(Debug, Error)]
enum Error {
    /// The payload within the Flight ticket cannot be deserialised into a
    /// [`proto::IngesterQueryRequest`].
    #[error("invalid flight ticket: {0}")]
    InvalidTicket(#[from] prost::DecodeError),

    /// The number of simultaneous queries being executed has been reached.
    #[error("simultaneous query limit exceeded")]
    RequestLimit,
}

/// Map a query-execution error into a [`tonic::Status`].
impl From<QueryError> for tonic::Status {
    fn from(e: QueryError) -> Self {
        use tonic::Code;

        let code = match e {
            QueryError::TableNotFound(_, _) | QueryError::NamespaceNotFound(_) => Code::NotFound,
        };

        Self::new(code, e.to_string())
    }
}

/// Map a gRPC handler error to a [`tonic::Status`].
impl From<Error> for tonic::Status {
    fn from(e: Error) -> Self {
        use tonic::Code;

        let code = match e {
            Error::InvalidTicket(_) => {
                debug!(error=%e, "invalid flight query ticket");
                Code::InvalidArgument
            }
            Error::RequestLimit => {
                warn!("simultaneous query limit exceeded");
                Code::ResourceExhausted
            }
        };

        Self::new(code, e.to_string())
    }
}

/// Concrete implementation of the gRPC Arrow Flight Service API
#[derive(Debug)]
pub(crate) struct FlightService<Q> {
    query_handler: Q,

    /// A request limiter to restrict the number of simultaneous requests this
    /// ingester services.
    ///
    /// This allows the ingester to drop a portion of requests when experiencing
    /// an unusual flood of requests
    request_sem: Semaphore,

    /// Number of queries rejected due to lack of available `request_sem`
    /// permit.
    query_request_limit_rejected: U64Counter,

    ingester_id: IngesterId,
}

impl<Q> FlightService<Q> {
    pub(super) fn new(
        query_handler: Q,
        ingester_id: IngesterId,
        max_simultaneous_requests: usize,
        metrics: &metric::Registry,
    ) -> Self {
        let query_request_limit_rejected = metrics
            .register_metric::<U64Counter>(
                "query_request_limit_rejected",
                "number of query requests rejected due to exceeding parallel request limit",
            )
            .recorder(&[]);

        Self {
            query_handler,
            request_sem: Semaphore::new(max_simultaneous_requests),
            query_request_limit_rejected,
            ingester_id,
        }
    }
}

type TonicStream<T> = Pin<Box<dyn Stream<Item = Result<T, tonic::Status>> + Send + 'static>>;

#[tonic::async_trait]
impl<Q> Flight for FlightService<Q>
where
    Q: QueryExec<Response = QueryResponse> + 'static,
{
    type HandshakeStream = TonicStream<HandshakeResponse>;
    type ListFlightsStream = TonicStream<FlightInfo>;
    type DoGetStream = TonicStream<FlightData>;
    type DoPutStream = TonicStream<PutResult>;
    type DoActionStream = TonicStream<arrow_flight::Result>;
    type ListActionsStream = TonicStream<ActionType>;
    type DoExchangeStream = TonicStream<FlightData>;

    async fn get_schema(
        &self,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<SchemaResult>, tonic::Status> {
        Err(tonic::Status::unimplemented("Not yet implemented"))
    }

    async fn do_get(
        &self,
        request: Request<Ticket>,
    ) -> Result<Response<Self::DoGetStream>, tonic::Status> {
        let span_ctx: Option<SpanContext> = request.extensions().get().cloned();
        let span = span_ctx.child_span("ingester query");

        // Acquire and hold a permit for the duration of this request, or return
        // an error if the existing requests have already exhausted the
        // allocation.
        //
        // Our goal is to limit the number of concurrently executing queries as
        // a rough way of ensuring we don't explode memory by trying to do too
        // much at the same time.
        let _permit = match self.request_sem.try_acquire() {
            Ok(p) => p,
            Err(TryAcquireError::NoPermits) => {
                warn!("simultaneous request limit exceeded - dropping query request");
                self.query_request_limit_rejected.inc(1);
                return Err(Error::RequestLimit)?;
            }
            Err(e) => panic!("request limiter error: {e}"),
        };

        let ticket = request.into_inner();
        let request = proto::IngesterQueryRequest::decode(&*ticket.ticket).map_err(Error::from)?;

        // Extract the namespace/table identifiers
        let namespace_id = NamespaceId::new(request.namespace_id);
        let table_id = TableId::new(request.table_id);

        // Predicate pushdown is part of the API, but not implemented.
        if let Some(p) = request.predicate {
            debug!(predicate=?p, "ignoring query predicate (unsupported)");
        }

        let response = match self
            .query_handler
            .query_exec(namespace_id, table_id, request.columns, span)
            .await
        {
            Ok(v) => v,
            Err(e @ (QueryError::TableNotFound(_, _) | QueryError::NamespaceNotFound(_))) => {
                debug!(
                        error=%e,
                        %namespace_id,
                        %table_id,
                        "query error, no buffered data found");

                return Err(e)?;
            }
        };

        let output = encode_response(response, self.ingester_id).map_err(tonic::Status::from);

        Ok(Response::new(Box::pin(output) as Self::DoGetStream))
    }

    async fn handshake(
        &self,
        request: Request<Streaming<HandshakeRequest>>,
    ) -> Result<Response<Self::HandshakeStream>, tonic::Status> {
        let request = request.into_inner().message().await?.unwrap();
        let response = HandshakeResponse {
            protocol_version: request.protocol_version,
            payload: request.payload,
        };
        let output = futures::stream::iter(std::iter::once(Ok(response)));
        Ok(Response::new(Box::pin(output) as Self::HandshakeStream))
    }

    async fn list_flights(
        &self,
        _request: Request<Criteria>,
    ) -> Result<Response<Self::ListFlightsStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("Not yet implemented"))
    }

    async fn get_flight_info(
        &self,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, tonic::Status> {
        Err(tonic::Status::unimplemented("Not yet implemented"))
    }

    async fn do_put(
        &self,
        _request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoPutStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("Not yet implemented"))
    }

    async fn do_action(
        &self,
        _request: Request<Action>,
    ) -> Result<Response<Self::DoActionStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("Not yet implemented"))
    }

    async fn list_actions(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<Self::ListActionsStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("Not yet implemented"))
    }

    async fn do_exchange(
        &self,
        _request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoExchangeStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("Not yet implemented"))
    }
}

/// Encode the partition information as a None flight data with meatadata
fn encode_partition(
    // Partition ID.
    partition_id: PartitionId,
    // Count of persisted Parquet files for the [`PartitionData`] instance this
    // [`PartitionResponse`] was generated from.
    //
    // [`PartitionData`]: crate::buffer_tree::partition::PartitionData
    // [`PartitionResponse`]: crate::query::partition_response::PartitionResponse
    completed_persistence_count: u64,
    ingester_id: IngesterId,
) -> Result<FlightData, FlightError> {
    let mut bytes = bytes::BytesMut::new();
    let app_metadata = proto::IngesterQueryResponseMetadata {
        partition_id: partition_id.get(),
        ingester_uuid: ingester_id.to_string(),
        completed_persistence_count,
    };
    prost::Message::encode(&app_metadata, &mut bytes)
        .map_err(|e| FlightError::from_external_error(Box::new(e)))?;

    Ok(FlightData::new(
        None,
        IpcMessage(build_none_flight_msg().into()),
        bytes.to_vec(),
        vec![],
    ))
}

fn build_none_flight_msg() -> Vec<u8> {
    let mut fbb = FlatBufferBuilder::new();

    let mut message = arrow::ipc::MessageBuilder::new(&mut fbb);
    message.add_version(arrow::ipc::MetadataVersion::V5);
    message.add_header_type(arrow::ipc::MessageHeader::NONE);
    message.add_bodyLength(0);

    let data = message.finish();
    fbb.finish(data, None);

    fbb.finished_data().to_vec()
}

/// Converts a QueryResponse into a stream of Arrow Flight [`FlightData`] response frames.
fn encode_response(
    response: QueryResponse,
    ingester_id: IngesterId,
) -> impl Stream<Item = Result<FlightData, FlightError>> {
    response.into_partition_stream().flat_map(move |partition| {
        let partition_id = partition.id();
        let completed_persistence_count = partition.completed_persistence_count();

        // prefix payload data w/ metadata for that particular partition
        let head = futures::stream::once(async move {
            encode_partition(partition_id, completed_persistence_count, ingester_id)
        });

        // An output vector of FlightDataEncoder streams, each entry stream with
        // a differing schema.
        //
        // Optimized for the common case of there being a single consistent
        // schema across all batches (1 stream).
        let mut output = Vec::with_capacity(1);

        let mut batch_iter = partition.into_record_batches().into_iter().peekable();

        // While there are more batches to process.
        while let Some(schema) = batch_iter.peek().map(|v| v.schema()) {
            output.push(
                FlightDataEncoderBuilder::new().build(futures::stream::iter(
                    // Take all the RecordBatch with a matching schema
                    std::iter::from_fn(|| batch_iter.next_if(|v| v.schema() == schema))
                        .map(Ok)
                        .collect::<Vec<Result<_, FlightError>>>(),
                )),
            )
        }

        head.chain(futures::stream::iter(output).flatten())
    })
}

#[cfg(test)]
mod tests {
    use arrow::array::{Float64Array, Int32Array};
    use arrow_flight::decode::{DecodedPayload, FlightRecordBatchStream};
    use assert_matches::assert_matches;
    use bytes::Bytes;
    use tonic::Code;

    use crate::{
        make_batch,
        query::{
            mock_query_exec::MockQueryExec, partition_response::PartitionResponse,
            response::PartitionStream,
        },
    };

    use super::*;

    #[tokio::test]
    async fn limits_concurrent_queries() {
        let mut flight = FlightService::new(
            MockQueryExec::default(),
            IngesterId::new(),
            100,
            &metric::Registry::default(),
        );

        let req = tonic::Request::new(Ticket {
            ticket: Bytes::new(),
        });
        match flight.do_get(req).await {
            Ok(_) => panic!("expected error because of invalid ticket"),
            Err(s) => {
                assert_eq!(s.code(), Code::NotFound); // Mock response value
            }
        }

        flight.request_sem = Semaphore::new(0);

        let req = tonic::Request::new(Ticket {
            ticket: Bytes::new(),
        });
        match flight.do_get(req).await {
            Ok(_) => panic!("expected error because of request limit"),
            Err(s) => {
                assert_eq!(s.code(), Code::ResourceExhausted);
            }
        }
    }

    /// Regression test for https://github.com/influxdata/idpe/issues/17408
    #[tokio::test]
    async fn test_chunks_with_different_schemas() {
        let ingester_id = IngesterId::new();
        let (batch1, schema1) = make_batch!(
            Float64Array("float" => vec![1.1, 2.2, 3.3]),
            Int32Array("int" => vec![1, 2, 3]),
        );
        let (batch2, schema2) = make_batch!(
            Float64Array("float" => vec![4.4]),
            Int32Array("int" => vec![4]),
        );
        assert_eq!(schema1, schema2);
        let (batch3, schema3) = make_batch!(
            Int32Array("int" => vec![5, 6]),
        );
        let (batch4, schema4) = make_batch!(
            Float64Array("float" => vec![7.7]),
            Int32Array("int" => vec![8]),
        );
        assert_eq!(schema1, schema4);

        let flight = FlightService::new(
            MockQueryExec::default().with_result(Ok(QueryResponse::new(PartitionStream::new(
                futures::stream::iter([PartitionResponse::new(
                    vec![
                        batch1.clone(),
                        batch2.clone(),
                        batch3.clone(),
                        batch4.clone(),
                    ],
                    PartitionId::new(2),
                    42,
                )]),
            )))),
            ingester_id,
            100,
            &metric::Registry::default(),
        );

        let req = tonic::Request::new(Ticket {
            ticket: Bytes::new(),
        });
        let response_stream = flight
            .do_get(req)
            .await
            .unwrap()
            .into_inner()
            .map_err(FlightError::Tonic);
        let flight_decoder =
            FlightRecordBatchStream::new_from_flight_data(response_stream).into_inner();
        let flight_data = flight_decoder.try_collect::<Vec<_>>().await.unwrap();
        assert_eq!(flight_data.len(), 8);

        // partition info
        assert_matches!(flight_data[0].payload, DecodedPayload::None);
        let md_actual =
            proto::IngesterQueryResponseMetadata::decode(flight_data[0].app_metadata()).unwrap();
        let md_expected = proto::IngesterQueryResponseMetadata {
            partition_id: 2,
            ingester_uuid: ingester_id.to_string(),
            completed_persistence_count: 42,
        };
        assert_eq!(md_actual, md_expected);

        // first & second chunk
        match &flight_data[1].payload {
            DecodedPayload::Schema(actual) => {
                assert_eq!(actual, &schema1);
            }
            other => {
                panic!("Unexpected payload: {other:?}");
            }
        }
        match &flight_data[2].payload {
            DecodedPayload::RecordBatch(actual) => {
                assert_eq!(actual, &batch1);
            }
            other => {
                panic!("Unexpected payload: {other:?}");
            }
        }
        match &flight_data[3].payload {
            DecodedPayload::RecordBatch(actual) => {
                assert_eq!(actual, &batch2);
            }
            other => {
                panic!("Unexpected payload: {other:?}");
            }
        }

        // third chunk
        match &flight_data[4].payload {
            DecodedPayload::Schema(actual) => {
                assert_eq!(actual, &schema3);
            }
            other => {
                panic!("Unexpected payload: {other:?}");
            }
        }
        match &flight_data[5].payload {
            DecodedPayload::RecordBatch(actual) => {
                assert_eq!(actual, &batch3);
            }
            other => {
                panic!("Unexpected payload: {other:?}");
            }
        }

        // forth chunk
        match &flight_data[6].payload {
            DecodedPayload::Schema(actual) => {
                assert_eq!(actual, &schema4);
            }
            other => {
                panic!("Unexpected payload: {other:?}");
            }
        }
        match &flight_data[7].payload {
            DecodedPayload::RecordBatch(actual) => {
                assert_eq!(actual, &batch4);
            }
            other => {
                panic!("Unexpected payload: {other:?}");
            }
        }
    }
}