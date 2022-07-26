#[cfg(feature = "embedded-svc")]
pub use embedded_svc_compat::*;

#[cfg(feature = "embedded-svc")]
mod embedded_svc_compat {
    use core::future::{pending, Future};

    use embedded_io::asynch::{Read, Write};
    use embedded_io::Io;

    use embedded_svc::http::headers::{content_len, content_type, ContentLenParseBuf};
    use embedded_svc::http::server::asynch::{Connection, Handler};
    use embedded_svc::http::server::HandlerResult;
    use embedded_svc::mutex::{RawMutex, StdRawMutex};
    use embedded_svc::utils::asynch::mpmc::Channel;
    use embedded_svc::utils::asynch::select::{select3, select_all_hvec};
    use embedded_svc::utils::http::server::registration::asynch::{
        HandlerRegistration, ServerHandler,
    };

    use crate::asynch::http::{
        send_headers, send_headers_end, send_status, Body, BodyType, Error, Method, Request,
        SendBody,
    };
    use crate::asynch::stdnal::StdTcpAcceptor;
    use crate::asynch::tcp::TcpAcceptor;

    //////

    // pub struct Request1<T>(T);

    // pub trait Handler<T>
    // where
    //     T: Read,
    // {
    //     type HandleFuture<'a>: Future<Output = ()> where Self: 'a;

    //     fn handle(&self, request: Request1<T>) -> Self::HandleFuture<'_>;
    // }

    // impl<T> Handler<T> for ()
    // where T: Read,
    // {
    //     type HandleFuture<'a> where Self: 'a = impl Future<Output = ()>;

    //     fn handle(&self, request: Request1<T>) -> Self::HandleFuture<'_> {
    //         async move {

    //         }
    //     }
    // }
    // pub async fn test0() {
    //     let conn = conn().await;

    //     test(conn, ()).await;
    // }

    // pub async fn test<T, H>(mut connection: T, handler: H)
    // where
    //     H: for <'a> Handler<&'a mut T>,
    //     T: Read + 'static,
    // {
    //     loop {
    //         handler.handle(Request1(&mut connection)).await;

    //         handler.handle(Request1(&mut connection)).await;
    //     }
    // }

    // pub async fn conn() -> impl Read {
    // }

    ///////

    struct PrivateData;

    pub struct ServerRequest(PrivateData);

    pub struct ServerResponse(PrivateData);

    pub enum ServerConnection<'b, const N: usize, T> {
        RequestState(Option<ServerRequestState<'b, N, T>>),
        ResponseState(Option<SendBody<T>>),
    }

    pub struct ServerRequestState<'b, const N: usize, T> {
        request: Request<'b, N>,
        io: Body<'b, T>,
    }

    impl<'b, const N: usize, T> ServerConnection<'b, N, T> {
        pub async fn new(
            buf: &'b mut [u8],
            mut io: T,
        ) -> Result<ServerConnection<'b, N, T>, Error<T::Error>>
        where
            T: Read + Write,
        {
            let mut raw_request = Request::new();

            let (buf, read_len) = raw_request.receive(buf, &mut io).await?;

            let body = Body::new(
                BodyType::from_headers(raw_request.headers.iter()),
                buf,
                read_len,
                io,
            );

            Ok(Self::RequestState(Some(ServerRequestState {
                request: raw_request,
                io: body,
            })))
        }

        fn request(&self) -> &ServerRequestState<'b, N, T> {
            match self {
                Self::RequestState(request) => request.as_ref().unwrap(),
                _ => unreachable!(),
            }
        }

        fn request_mut(&mut self) -> &mut ServerRequestState<'b, N, T> {
            match self {
                Self::RequestState(request) => request.as_mut().unwrap(),
                _ => unreachable!(),
            }
        }

        fn response_write(&mut self) -> &mut SendBody<T> {
            match self {
                Self::ResponseState(response_write) => response_write.as_mut().unwrap(),
                _ => unreachable!(),
            }
        }

        fn raw_io(&mut self) -> &mut T
        where
            T: Read + Write,
        {
            match self {
                Self::RequestState(request) => request.as_mut().unwrap().io.as_raw_reader(),
                Self::ResponseState(response_write) => {
                    response_write.as_mut().unwrap().as_raw_writer()
                }
            }
        }

        async fn complete_request<'a>(
            &'a mut self,
            buf: &'a mut [u8],
            status: Option<u16>,
            reason: Option<&'a str>,
            headers: &'a [(&'a str, &'a str)],
        ) -> Result<Option<&'a mut SendBody<T>>, Error<T::Error>>
        where
            T: Read + Write,
        {
            match self {
                Self::RequestState(request) => {
                    let io = &mut request.as_mut().unwrap().io;

                    while io.read(buf).await? > 0 {}
                    let request = request.take().unwrap();

                    let mut io = request.io.release();

                    send_status(status, reason, &mut io).await?;
                    let body_type = send_headers(headers.iter(), &mut io).await?;
                    send_headers_end(&mut io).await?;

                    let io = SendBody::new(body_type, io);

                    *self = Self::ResponseState(Some(io));

                    Ok(Some(self.response_write()))
                }
                Self::ResponseState(_) => Ok(None),
            }
        }

        async fn complete_response<'a>(
            &'a mut self,
            buf: &'a mut [u8],
            status: Option<u16>,
            reason: Option<&'a str>,
            headers: &'a [(&'a str, &'a str)],
        ) -> Result<bool, Error<T::Error>>
        where
            T: Read + Write,
        {
            if let Some(body) = self.complete_request(buf, status, reason, headers).await? {
                body.finish().await?;

                Ok(true)
            } else {
                Ok(false)
            }
        }

        async fn complete_err<'a>(
            &'a mut self,
            buf: &'a mut [u8],
            err_str: &'a str,
        ) -> Result<bool, Error<T::Error>>
        where
            T: Read + Write,
        {
            let mut clbuf = ContentLenParseBuf::new();
            let headers = [
                content_len(err_str.as_bytes().len() as u64, &mut clbuf),
                content_type("text/plain"),
            ];

            if let Some(body) = self
                .complete_request(buf, Some(500), Some("Internal Error"), &headers)
                .await?
            {
                body.write_all(err_str.as_bytes()).await?;

                Ok(true)
            } else {
                Ok(false)
            }
        }
    }

    impl<'b, const N: usize, T> Io for ServerConnection<'b, N, T>
    where
        T: Io,
    {
        type Error = Error<T::Error>;
    }

    impl<'b, const N: usize, T> Connection for ServerConnection<'b, N, T>
    where
        T: Read + Write,
    {
        type Request = ServerRequest;

        type Response = ServerResponse;

        type Headers = Request<'b, N>;

        type Read = Body<'b, T>;

        type Write = SendBody<T>;

        type RawConnectionError = T::Error;

        type RawConnection = T;

        type IntoResponseFuture<'a>
        where
            Self: 'a,
        = impl Future<Output = Result<Self::Response, Self::Error>>;

        fn split<'a>(
            &'a mut self,
            _request: &'a mut Self::Request,
        ) -> (&'a Self::Headers, &'a mut Self::Read) {
            let req = self.request_mut();

            (&req.request, &mut req.io)
        }

        fn headers<'a>(&'a self, _request: &'a Self::Request) -> &'a Self::Headers {
            &self.request().request
        }

        fn into_response<'a>(
            &'a mut self,
            _request: Self::Request,
            status: u16,
            message: Option<&'a str>,
            headers: &'a [(&'a str, &'a str)],
        ) -> Self::IntoResponseFuture<'a> {
            async move {
                let mut buf = [0_u8; 1024]; // TODO
                self.complete_request(&mut buf, Some(status), message, headers)
                    .await?;

                Ok(ServerResponse(PrivateData))
            }
        }

        fn writer<'a>(&'a mut self, _response: &'a mut Self::Response) -> &'a mut Self::Write {
            self.response_write()
        }

        fn raw_connection(&mut self) -> Result<&mut Self::RawConnection, Self::Error> {
            Ok(self.raw_io())
        }
    }

    pub async fn handle_connection<const N: usize, const B: usize, T, H>(
        mut io: T,
        handler: &H,
    ) -> Result<(), Error<T::Error>>
    where
        H: for<'b> HandlerRegistration<ServerConnection<'b, N, &'b mut T>>,
        T: Read + Write,
    {
        let mut buf = [0_u8; B];

        loop {
            handle_request::<N, _, _>(&mut buf, &mut io, handler).await?;
        }
    }

    pub async fn handle_request<'b, const N: usize, H, T>(
        buf: &'b mut [u8],
        io: T,
        handler: &H,
    ) -> Result<(), Error<T::Error>>
    where
        H: HandlerRegistration<ServerConnection<'b, N, T>>,
        T: Read + Write,
    {
        let mut connection = ServerConnection::new(buf, io).await?;

        let path = ""; // TODO connection.request().request().request.path.unwrap_or("");
        let result = if let Some(method) = connection.request().request.method {
            handler
                .handle(
                    false,
                    path,
                    method.into(),
                    &mut connection,
                    ServerRequest(PrivateData),
                )
                .await
        } else {
            ().handle(
                true,
                path,
                Method::Get.into(),
                &mut connection,
                ServerRequest(PrivateData),
            )
            .await
        };

        let mut buf = [0_u8; 64];

        let completed = match result {
            Result::Ok(_) => {
                connection
                    .complete_response(&mut buf, Some(200), Some("OK"), &[])
                    .await?
            }
            Result::Err(e) => connection.complete_err(&mut buf, e.message()).await?,
        };

        if completed {
            Ok(())
        } else {
            Err(Error::IncompleteBody)
        }
    }

    pub struct Server<const N: usize, const B: usize, A, H> {
        acceptor: A,
        handler: H,
    }

    pub struct Simple;

    impl<C> Handler<C> for Simple
    where
        C: Connection,
    {
        type HandleFuture<'a>
        where
            Self: 'a,
            C: 'a,
        = impl Future<Output = HandlerResult>;

        fn handle<'a>(
            &'a self,
            connection: &'a mut C,
            request: <C as Connection>::Request,
        ) -> Self::HandleFuture<'a> {
            async move { Ok(()) }
        }
    }

    pub struct Simple2;

    impl<C> Handler<C> for Simple2
    where
        C: Connection,
    {
        type HandleFuture<'a>
        where
            Self: 'a,
            C: 'a,
        = impl Future<Output = HandlerResult>;

        fn handle<'a>(
            &'a self,
            connection: &'a mut C,
            request: <C as Connection>::Request,
        ) -> Self::HandleFuture<'a> {
            async move {
                connection.into_ok_response(request).await?;

                Ok(())
            }
        }
    }

    pub async fn test_std(acceptor: StdTcpAcceptor) {
        test::<StdTcpAcceptor, StdRawMutex>(acceptor).await;
    }

    pub async fn test<A, R>(acceptor: A)
    where
        A: TcpAcceptor,
        R: RawMutex,
    {
        let handler = RootHandlerChainBuilder::new()
            .at("/")
            .get(Simple)
            .post(Simple2)
            .at("/foo")
            .get(Simple2)
            .create();

        let mut server = Server::<1, 1, _, _>::new(acceptor, handler);

        server.process::<1, 1, R, _>(pending()).await.unwrap();
    }

    impl<const N: usize, const B: usize, A, H> Server<N, B, A, H>
    where
        A: TcpAcceptor,
        H: for<'t, 'b> HandlerRegistration<
            ServerConnection<'b, N, &'b mut <A as TcpAcceptor>::Connection<'t>>,
        >,
    {
        pub const fn new(acceptor: A, handler: H) -> Self {
            Self { acceptor, handler }
        }

        pub async fn process<
            const P: usize,
            const W: usize,
            R: RawMutex,
            Q: Future<Output = ()>,
        >(
            &mut self,
            quit: Q,
        ) -> Result<(), Error<A::Error>> {
            let channel = Channel::<R, _, W>::new();
            let mut handlers = heapless::Vec::<_, P>::new();

            for _ in 0..P {
                handlers
                    .push(async {
                        loop {
                            let io = channel.recv().await;

                            handle_connection::<N, B, _, _>(io, &self.handler)
                                .await
                                .unwrap();
                        }
                    })
                    .map_err(|_| ())
                    .unwrap();
            }

            select3(
                quit,
                async {
                    loop {
                        let io = self.acceptor.accept().await.map_err(Error::Io).unwrap();

                        channel.send(io).await;
                    }
                },
                select_all_hvec(handlers),
            )
            .await;

            Ok(())
        }
    }

    pub struct HandlerChain<H, N> {
        pub path: &'static str,
        pub method: embedded_svc::http::Method,
        pub handler: H,
        pub next: N,
    }

    impl<H, N> HandlerChain<H, N> {
        pub const fn new(
            path: &'static str,
            method: embedded_svc::http::Method,
            handler: H,
            next: N,
        ) -> Self {
            Self {
                path,
                method,
                handler,
                next,
            }
        }
    }

    impl<C, H, N> HandlerRegistration<C> for HandlerChain<H, N>
    where
        C: Connection,
        H: Handler<C>,
        N: HandlerRegistration<C>,
    {
        type HandleFuture<'a>
        where
            Self: 'a,
            C: 'a,
        = impl Future<Output = HandlerResult>;

        fn handle<'a>(
            &'a self,
            path_registered: bool,
            path: &'a str,
            method: embedded_svc::http::Method,
            connection: &'a mut C,
            request: C::Request,
        ) -> Self::HandleFuture<'a> {
            async move {
                if self.path == path && self.method == method {
                    self.handler.handle(connection, request).await
                } else {
                    self.next
                        .handle(path_registered, path, method, connection, request)
                        .await
                }
            }
        }
    }

    pub struct HandlerChainBuilder<H> {
        path: &'static str,
        handler: H,
    }

    impl HandlerChainBuilder<()> {
        fn empty(path: &'static str) -> Self {
            Self { path, handler: () }
        }
    }

    impl<H> HandlerChainBuilder<H> {
        pub fn get<H2>(self, handler: H2) -> HandlerChainBuilder<HandlerChain<H2, H>> {
            self.method(embedded_svc::http::Method::Get, handler)
        }

        pub fn post<H2>(self, handler: H2) -> HandlerChainBuilder<HandlerChain<H2, H>> {
            self.method(embedded_svc::http::Method::Post, handler)
        }

        pub fn put<H2>(self, handler: H2) -> HandlerChainBuilder<HandlerChain<H2, H>> {
            self.method(embedded_svc::http::Method::Put, handler)
        }

        pub fn delete<H2>(self, handler: H2) -> HandlerChainBuilder<HandlerChain<H2, H>> {
            self.method(embedded_svc::http::Method::Delete, handler)
        }

        pub fn method<H2>(
            self,
            method: embedded_svc::http::Method,
            handler: H2,
        ) -> HandlerChainBuilder<HandlerChain<H2, H>> {
            HandlerChainBuilder {
                path: self.path,
                handler: HandlerChain::new(self.path, method, handler, self.handler),
            }
        }

        pub fn at(self, path: &'static str) -> HandlerChainBuilder<H> {
            HandlerChainBuilder {
                path,
                handler: self.handler,
            }
        }

        pub fn create(self) -> H {
            self.handler
        }
    }

    pub struct RootHandlerChainBuilder<H>(H);

    impl RootHandlerChainBuilder<()> {
        pub const fn new() -> Self {
            Self(())
        }

        pub fn at(self, path: &'static str) -> HandlerChainBuilder<()> {
            HandlerChainBuilder::empty(path)
        }

        pub fn create(self) -> () {
            self.0
        }
    }
}
