use std::{any::type_name, borrow::Cow, marker::PhantomData, pin::Pin, sync::Arc};

use serde::de::DeserializeOwned;
use specta::Type;

use crate::{
    internal::{
        BaseMiddleware, BuiltProcedureBuilder, Layer, LayerResult, MiddlewareLayerBuilder,
        ProcedureKind, RequestContext, ResolverLayer, UnbuiltProcedureBuilder,
    },
    typedef, ExecError, MiddlewareBuilder, MiddlewareLike, RequestLayer, SerializeMarker,
    StreamRequestLayer,
};

use super::{
    AlphaMiddlewareBuilder, AlphaMiddlewareLike, IntoProcedure, IntoProcedureCtx,
    MiddlewareArgMapper, Mw, ProcedureLike, RequestKind, RequestLayerMarker, StreamLayerMarker,
};

/// This exists solely to make Rust shut up about unconstrained generic types

pub trait ResolverFunction<TMarker>: Send + Sync + 'static {
    type LayerCtx: Send + Sync + 'static;
    type Arg: DeserializeOwned + Type;
    type RequestMarker;
    type Result;
    type ResultMarker;

    fn exec(&self, ctx: Self::LayerCtx, arg: Self::Arg) -> Self::Result;
}

pub struct Marker<A, B, C, D>(PhantomData<(A, B, C, D)>);

impl<
        TLayerCtx,
        TArg,
        TResult,
        TResultMarker,
        F: Fn(TLayerCtx, TArg) -> TResult + Send + Sync + 'static,
    > ResolverFunction<RequestLayerMarker<Marker<TArg, TResult, TResultMarker, TLayerCtx>>> for F
where
    TArg: DeserializeOwned + Type,
    TResult: RequestLayer<TResultMarker>,
    TLayerCtx: Send + Sync + 'static,
{
    type LayerCtx = TLayerCtx;
    type Arg = TArg;
    type Result = TResult;
    type ResultMarker = RequestLayerMarker<TResultMarker>;
    type RequestMarker = TResultMarker;

    fn exec(&self, ctx: Self::LayerCtx, arg: Self::Arg) -> Self::Result {
        self(ctx, arg)
    }
}

impl<
        TLayerCtx,
        TArg,
        TResult,
        TResultMarker,
        F: Fn(TLayerCtx, TArg) -> TResult + Send + Sync + 'static,
    > ResolverFunction<StreamLayerMarker<Marker<TArg, TResult, TResultMarker, TLayerCtx>>> for F
where
    TArg: DeserializeOwned + Type,
    TResult: StreamRequestLayer<TResultMarker>,
    TLayerCtx: Send + Sync + 'static,
{
    type LayerCtx = TLayerCtx;
    type Arg = TArg;
    type Result = TResult;
    type ResultMarker = StreamLayerMarker<TResultMarker>;
    type RequestMarker = TResultMarker;

    fn exec(&self, ctx: Self::LayerCtx, arg: Self::Arg) -> Self::Result {
        self(ctx, arg)
    }
}

pub struct MissingResolver<TLayerCtx> {
    phantom: PhantomData<TLayerCtx>,
}

impl<TLayerCtx> Default for MissingResolver<TLayerCtx> {
    fn default() -> Self {
        Self {
            phantom: PhantomData,
        }
    }
}

impl<TLayerCtx> ResolverFunction<()> for MissingResolver<TLayerCtx>
where
    TLayerCtx: Send + Sync + 'static,
{
    type LayerCtx = TLayerCtx;
    type Arg = ();
    type Result = ();
    type ResultMarker = RequestLayerMarker<SerializeMarker>;
    type RequestMarker = SerializeMarker;

    fn exec(&self, _: Self::LayerCtx, _: Self::Arg) -> Self::Result {
        unreachable!();
    }
}

// TODO: `.with` but only support BEFORE resolver is set by the user.

// TODO: Check metadata stores on this so plugins can extend it to do cool stuff
// TODO: Logical order for these generics cause right now it's random
pub struct AlphaProcedure<R, RMarker, TMiddleware>(
    // Is `None` after `.build()` is called. `.build()` can't take `self` cause dyn safety.
    Option<R>,
    TMiddleware,
    RMarker,
    PhantomData<(RMarker)>,
)
where
    R: ResolverFunction<RMarker>,
    TMiddleware: AlphaMiddlewareBuilderLike;

impl<TMiddleware, R, RMarker> AlphaProcedure<R, RMarker, TMiddleware>
where
    TMiddleware: AlphaMiddlewareBuilderLike,
    R: ResolverFunction<RMarker>,
{
    pub fn new_from_resolver(k: RMarker, mw: TMiddleware, resolver: R) -> Self {
        Self(Some(resolver), mw, k, PhantomData)
    }
}

impl<TCtx, TLayerCtx> AlphaProcedure<MissingResolver<TLayerCtx>, (), AlphaBaseMiddleware<TCtx>>
where
    TCtx: Send + Sync + 'static,
    TLayerCtx: Send + Sync + 'static,
{
    pub fn new_from_middleware<TMiddleware>(
        mw: TMiddleware,
    ) -> AlphaProcedure<MissingResolver<TLayerCtx>, (), TMiddleware>
    where
        TMiddleware: AlphaMiddlewareBuilderLike<Ctx = TCtx> + Send + 'static,
    {
        AlphaProcedure(Some(MissingResolver::default()), mw, (), PhantomData)
    }
}

impl<TMiddleware> AlphaProcedure<MissingResolver<TMiddleware::LayerCtx>, (), TMiddleware>
where
    TMiddleware: AlphaMiddlewareBuilderLike + Send + 'static,
{
    pub fn query<R, RMarker>(
        self,
        builder: R,
    ) -> AlphaProcedure<R, RequestLayerMarker<RMarker>, TMiddleware>
    where
        R: ResolverFunction<RequestLayerMarker<RMarker>, LayerCtx = TMiddleware::LayerCtx>
            + Fn(TMiddleware::LayerCtx, R::Arg) -> R::Result,
        R::Result: RequestLayer<R::RequestMarker>,
    {
        AlphaProcedure::new_from_resolver(
            RequestLayerMarker::new(RequestKind::Query),
            self.1,
            builder,
        )
    }

    pub fn mutation<R, RMarker>(
        self,
        builder: R,
    ) -> AlphaProcedure<R, RequestLayerMarker<RMarker>, TMiddleware>
    where
        R: ResolverFunction<RequestLayerMarker<RMarker>, LayerCtx = TMiddleware::LayerCtx>
            + Fn(TMiddleware::LayerCtx, R::Arg) -> R::Result,
        R::Result: RequestLayer<R::RequestMarker>,
    {
        AlphaProcedure::new_from_resolver(
            RequestLayerMarker::new(RequestKind::Mutation),
            self.1,
            builder,
        )
    }

    pub fn subscription<R, RMarker>(
        self,
        builder: R,
    ) -> AlphaProcedure<R, StreamLayerMarker<RMarker>, TMiddleware>
    where
        R: ResolverFunction<StreamLayerMarker<RMarker>, LayerCtx = TMiddleware::LayerCtx>
            + Fn(TMiddleware::LayerCtx, R::Arg) -> R::Result,
        R::Result: StreamRequestLayer<R::RequestMarker>,
    {
        AlphaProcedure::new_from_resolver(StreamLayerMarker::new(), self.1, builder)
    }
}

impl<R, RMarker, TMiddleware> AlphaProcedure<R, RMarker, TMiddleware>
where
    TMiddleware: AlphaMiddlewareBuilderLike,
    R: ResolverFunction<RMarker, LayerCtx = TMiddleware::LayerCtx>,
{
    pub fn with<TNewMiddleware>(
        self,
        builder: impl Fn(
            AlphaMiddlewareBuilder<TMiddleware::LayerCtx, TMiddleware::MwMapper, ()>,
        ) -> TNewMiddleware, // TODO: Remove builder closure
    ) -> AlphaProcedure<
        MissingResolver<TNewMiddleware::NewCtx>,
        (),
        AlphaMiddlewareLayerBuilder<TMiddleware, TNewMiddleware>,
    >
    where
        TNewMiddleware: AlphaMiddlewareLike<LayerCtx = TMiddleware::LayerCtx>,
    {
        let mw = builder(AlphaMiddlewareBuilder(PhantomData));
        AlphaProcedure::new_from_middleware(AlphaMiddlewareLayerBuilder {
            middleware: self.1,
            mw,
        })
    }
}

// TODO: Only do this impl when `R` is not `MissingResolver`!!!!!
impl<R, RMarker, TMiddleware> IntoProcedure<TMiddleware::Ctx>
    for AlphaProcedure<R, RequestLayerMarker<RMarker>, TMiddleware>
where
    R: ResolverFunction<RequestLayerMarker<RMarker>, LayerCtx = TMiddleware::LayerCtx>,
    RMarker: 'static,
    R::Result: RequestLayer<R::RequestMarker>,
    TMiddleware: AlphaMiddlewareBuilderLike,
{
    fn build(&mut self, key: Cow<'static, str>, ctx: &mut IntoProcedureCtx<'_, TMiddleware::Ctx>) {
        let resolver = Arc::new(self.0.take().expect("Called '.build()' multiple times!"));
        // TODO: Removing `Arc`?

        // serde_json::from_value::<TMiddleware::ArgMap<R::Arg>>();

        let m = match self.2.kind() {
            RequestKind::Query => &mut ctx.queries,
            RequestKind::Mutation => &mut ctx.mutations,
        };

        m.append(
            key.into(),
            self.1.build(AlphaResolverLayer {
                func: move |ctx, input, _| {
                    resolver
                        .exec(
                            ctx,
                            serde_json::from_value(input)
                                .map_err(ExecError::DeserializingArgErr)?,
                        )
                        .into_layer_result()
                },
                phantom: PhantomData,
            }),
            typedef::<
                <TMiddleware::MwMapper as MiddlewareArgMapper>::Input<R::Arg>,
                <<R as ResolverFunction<RequestLayerMarker<RMarker>>>::Result as RequestLayer<
                    R::RequestMarker,
                >>::Result,
            >(ctx.ty_store),
        );
    }
}

// TODO: Only do this impl when `R` is not `MissingResolver`!!!!!
impl<R, RMarker, TMiddleware> IntoProcedure<TMiddleware::Ctx>
    for AlphaProcedure<R, StreamLayerMarker<RMarker>, TMiddleware>
where
    R: ResolverFunction<StreamLayerMarker<RMarker>, LayerCtx = TMiddleware::LayerCtx>,
    RMarker: 'static,
    R::Result: StreamRequestLayer<R::RequestMarker>,
    TMiddleware: AlphaMiddlewareBuilderLike,
{
    fn build(&mut self, key: Cow<'static, str>, ctx: &mut IntoProcedureCtx<'_, TMiddleware::Ctx>) {
        let resolver = Arc::new(self.0.take().expect("Called '.build()' multiple times!")); // TODO: Removing `Arc`?

        // serde_json::from_value::<TMiddleware::ArgMap<R::Arg>>();

        ctx.subscriptions.append(
            key.into(),
            self.1.build(AlphaResolverLayer {
                func: move |ctx, input, _| {
                    resolver
                        .exec(
                            ctx,
                            serde_json::from_value(input)
                                .map_err(ExecError::DeserializingArgErr)?,
                        )
                        .into_layer_result()
                },
                phantom: PhantomData,
            }),
            typedef::<
                <TMiddleware::MwMapper as MiddlewareArgMapper>::Input<R::Arg>,
                <<R as ResolverFunction<StreamLayerMarker<RMarker>>>::Result as StreamRequestLayer<R::RequestMarker>>::Result,
            >(ctx.ty_store),
        );
    }
}

// TODO: This only works without a resolver. `ProcedureLike` should work on `AlphaProcedure` without it but just without the `.query()` and `.mutate()` functions.
impl<TMiddleware> ProcedureLike<TMiddleware::LayerCtx>
    for AlphaProcedure<MissingResolver<TMiddleware::LayerCtx>, (), TMiddleware>
where
    TMiddleware: AlphaMiddlewareBuilderLike,
{
    type Middleware = TMiddleware;

    fn query<R, RMarker>(
        self,
        builder: R,
    ) -> AlphaProcedure<R, RequestLayerMarker<RMarker>, Self::Middleware>
    where
        R: ResolverFunction<RequestLayerMarker<RMarker>, LayerCtx = TMiddleware::LayerCtx>
            + Fn(TMiddleware::LayerCtx, R::Arg) -> R::Result,
        R::Result: RequestLayer<R::RequestMarker>,
    {
        AlphaProcedure::new_from_resolver(
            RequestLayerMarker::new(RequestKind::Query),
            self.1,
            builder,
        )
    }

    fn mutation<R, RMarker>(
        self,
        builder: R,
    ) -> AlphaProcedure<R, RequestLayerMarker<RMarker>, Self::Middleware>
    where
        R: ResolverFunction<RequestLayerMarker<RMarker>, LayerCtx = TMiddleware::LayerCtx>
            + Fn(TMiddleware::LayerCtx, R::Arg) -> R::Result,
        R::Result: RequestLayer<R::RequestMarker>,
    {
        AlphaProcedure::new_from_resolver(
            RequestLayerMarker::new(RequestKind::Query),
            self.1,
            builder,
        )
    }

    fn subscription<R, RMarker>(
        self,
        builder: R,
    ) -> AlphaProcedure<R, StreamLayerMarker<RMarker>, Self::Middleware>
    where
        R: ResolverFunction<StreamLayerMarker<RMarker>, LayerCtx = TMiddleware::LayerCtx>
            + Fn(TMiddleware::LayerCtx, R::Arg) -> R::Result,
        R::Result: StreamRequestLayer<R::RequestMarker>,
    {
        AlphaProcedure::new_from_resolver(StreamLayerMarker::new(), self.1, builder)
    }
}

///
/// `internal/middleware.rs`
///
use std::future::Future;

use futures::Stream;
use serde_json::Value;

pub trait AlphaMiddlewareBuilderLike: Send + 'static {
    type Ctx: Send + Sync + 'static;
    type LayerCtx: Send + Sync + 'static;
    type MwMapper: MiddlewareArgMapper;

    fn build<T>(&self, next: T) -> Box<dyn Layer<Self::Ctx>>
    where
        T: Layer<Self::LayerCtx>;
}

pub struct MiddlewareMerger<TMiddleware, TIncomingMiddleware>
where
    TMiddleware: AlphaMiddlewareBuilderLike,
    TIncomingMiddleware: AlphaMiddlewareBuilderLike<Ctx = TMiddleware::LayerCtx>,
{
    pub middleware: TMiddleware,
    pub middleware2: TIncomingMiddleware,
}

pub struct A<TPrev, TNext>(PhantomData<(TPrev, TNext)>)
where
    TPrev: MiddlewareArgMapper,
    TNext: MiddlewareArgMapper;

impl<TPrev, TNext> MiddlewareArgMapper for A<TPrev, TNext>
where
    TPrev: MiddlewareArgMapper,
    TNext: MiddlewareArgMapper,
{
    type Input<T> = TPrev::Input<TNext::Input<T>>
    where
        T: DeserializeOwned + Type + 'static;

    type Output<T> = TNext::Output<TPrev::Output<T>>
    where
        T: serde::Serialize;

    type State = TNext::State;

    fn map<T: serde::Serialize + DeserializeOwned + Type + 'static>(
        arg: Self::Input<T>,
    ) -> (Self::Output<T>, Self::State) {
        todo!()
    }
}

impl<TMiddleware, TIncomingMiddleware> AlphaMiddlewareBuilderLike
    for MiddlewareMerger<TMiddleware, TIncomingMiddleware>
where
    TMiddleware: AlphaMiddlewareBuilderLike,
    TIncomingMiddleware: AlphaMiddlewareBuilderLike<Ctx = TMiddleware::LayerCtx>,
{
    type Ctx = TMiddleware::Ctx;
    type LayerCtx = TIncomingMiddleware::LayerCtx;
    type MwMapper = A<TMiddleware::MwMapper, TIncomingMiddleware::MwMapper>;

    fn build<T>(&self, next: T) -> Box<dyn Layer<Self::Ctx>>
    where
        T: Layer<Self::LayerCtx>,
    {
        self.middleware.build(self.middleware2.build(next))
    }
}

pub struct AlphaMiddlewareLayerBuilder<TMiddleware, TNewMiddleware>
where
    TMiddleware: AlphaMiddlewareBuilderLike,
    TNewMiddleware: AlphaMiddlewareLike<LayerCtx = TMiddleware::LayerCtx>,
{
    pub middleware: TMiddleware,
    pub mw: TNewMiddleware,
}

impl<TMiddleware, TNewMiddleware> AlphaMiddlewareBuilderLike
    for AlphaMiddlewareLayerBuilder<TMiddleware, TNewMiddleware>
where
    TMiddleware: AlphaMiddlewareBuilderLike,
    TNewMiddleware: AlphaMiddlewareLike<LayerCtx = TMiddleware::LayerCtx>,
{
    type Ctx = TMiddleware::Ctx;
    type LayerCtx = TNewMiddleware::NewCtx;
    type MwMapper = A<TMiddleware::MwMapper, TNewMiddleware::MwMapper>;

    fn build<T>(&self, next: T) -> Box<dyn Layer<TMiddleware::Ctx>>
    where
        T: Layer<Self::LayerCtx> + Sync,
    {
        self.middleware.build(AlphaMiddlewareLayer {
            next: Arc::new(next),
            mw: self.mw.clone(),
        })
    }
}

pub struct AlphaMiddlewareLayer<TMiddleware, TNewMiddleware>
where
    TMiddleware: Layer<TNewMiddleware::NewCtx>,
    TNewMiddleware: AlphaMiddlewareLike,
{
    next: Arc<TMiddleware>, // TODO: Avoid arcing this if possible
    mw: TNewMiddleware,
}

impl<TMiddleware, TNewMiddleware> Layer<TNewMiddleware::LayerCtx>
    for AlphaMiddlewareLayer<TMiddleware, TNewMiddleware>
where
    TMiddleware: Layer<TNewMiddleware::NewCtx>,
    TNewMiddleware: AlphaMiddlewareLike,
{
    fn call(
        &self,
        ctx: TNewMiddleware::LayerCtx,
        input: Value,
        req: RequestContext,
    ) -> Result<LayerResult, ExecError> {
        self.mw.handle(ctx, input, req, self.next.clone())
    }
}

pub struct AlphaBaseMiddleware<TCtx>(PhantomData<TCtx>)
where
    TCtx: 'static;

impl<TCtx> Default for AlphaBaseMiddleware<TCtx>
where
    TCtx: 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<TCtx> AlphaBaseMiddleware<TCtx>
where
    TCtx: 'static,
{
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<TCtx> AlphaMiddlewareBuilderLike for AlphaBaseMiddleware<TCtx>
where
    TCtx: Send + Sync + 'static,
{
    type Ctx = TCtx;
    type LayerCtx = TCtx;
    type MwMapper = ();

    fn build<T>(&self, next: T) -> Box<dyn Layer<Self::Ctx>>
    where
        T: Layer<Self::LayerCtx>,
    {
        Box::new(next)
    }
}

pub struct AlphaResolverLayer<TLayerCtx, T>
where
    TLayerCtx: Send + Sync + 'static,
    T: Fn(TLayerCtx, Value, RequestContext) -> Result<LayerResult, ExecError>
        + Send
        + Sync
        + 'static,
{
    pub func: T,
    pub phantom: PhantomData<TLayerCtx>,
}

impl<T, TLayerCtx> Layer<TLayerCtx> for AlphaResolverLayer<TLayerCtx, T>
where
    TLayerCtx: Send + Sync + 'static,
    T: Fn(TLayerCtx, Value, RequestContext) -> Result<LayerResult, ExecError>
        + Send
        + Sync
        + 'static,
{
    fn call(&self, a: TLayerCtx, b: Value, c: RequestContext) -> Result<LayerResult, ExecError> {
        (self.func)(a, b, c)
    }
}
