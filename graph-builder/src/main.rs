// Copyright 2018 Alex Crawford
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use actix_service::Service;
use actix_web::{middleware, App, HttpServer};
use commons::metrics::{self, HasRegistry};
use commons::prelude_errors::*;
use commons::tracing::{get_context, get_tracer, init_tracer, set_span_tags};
use futures::future;
use graph_builder::{self, config, graph, status};
use log::{info};
use opentelemetry::{
    trace::{mark_span_as_active, FutureExt, Tracer},
    Context as ot_context,
};
use parking_lot::RwLock;
use std::collections::HashSet;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[actix_web::main]
async fn main() -> Result<(), Error> {
    let settings = config::AppSettings::assemble().context("could not assemble AppSettings")?;
    env_logger::Builder::from_default_env()
        .filter(Some(module_path!()), settings.verbosity)
        .filter(Some("cincinnati"), settings.verbosity)
        .init();
    info!("application settings:\n{:#?}", settings);

    let registry: prometheus::Registry =
        metrics::new_registry(Some(config::METRICS_PREFIX.to_string()))?;

    // Enable tracing
    init_tracer("graph-builder", settings.tracing_endpoint.clone())?;

    let plugins = settings.validate_and_build_plugins(Some(&registry))?;

    ensure_registered_metrics(
        &registry,
        config::METRICS_PREFIX,
        &settings.metrics_required,
    )?;

    let service_addr = (settings.address, settings.port);
    let public_addr = (settings.address, settings.public_port);
    let status_addr = (settings.status_address, settings.status_port);
    let app_prefix = settings.path_prefix.clone();
    let public_app_prefix = app_prefix.clone();

    // Shared state.
    let state = {
        let json_graph = Arc::new(RwLock::new(String::new()));
        let live = Arc::new(RwLock::new(false));
        let ready = Arc::new(RwLock::new(false));
        let secondary_metadata = Arc::new(RwLock::new(String::new()));
        graph::State::new(
            json_graph,
            settings.mandatory_client_parameters.clone(),
            live,
            ready,
            Box::leak(Box::new(plugins)),
            Box::leak(Box::new(registry)),
            secondary_metadata,
        )
    };

    // Graph scraper
    {
        let graph_state = state.clone();
        thread::spawn(move || {
            graph::run(&settings, &graph_state);
        });
    }

    // Status service.
    graph::register_metrics(state.registry())?;

    let status_state = state.clone();
    let metrics_server = HttpServer::new(move || {
        App::new()
            .app_data(actix_web::web::Data::new(status_state.clone()))
            .service(
                actix_web::web::resource("/liveness")
                    .route(actix_web::web::get().to(status::serve_liveness)),
            )
            .service(
                actix_web::web::resource("/metrics")
                    .route(actix_web::web::get().to(metrics::serve::<graph::State>)),
            )
            .service(
                actix_web::web::resource("/readiness")
                    .route(actix_web::web::get().to(status::serve_readiness)),
            )
    })
    .bind(status_addr)?
    .run();

    // Main service.
    let main_state = state.clone();
    let main_server = HttpServer::new(move || {
        App::new()
            .wrap(middleware::Compress::default())
            .wrap_fn(|req, srv| {
                let parent_context = get_context(&req);
                let mut span = get_tracer().start_with_context("request", parent_context);
                set_span_tags(req.path(), req.headers(), &mut span);
                let _active_span = mark_span_as_active(span);
                let cx = ot_context::current();
                srv.call(req).with_context(cx)
            })
            .app_data(actix_web::web::Data::new(main_state.clone()))
            .service(
                // keeping this for backward compatibility
                actix_web::web::resource(&format!("{}/v1/graph", app_prefix.clone()))
                    .route(actix_web::web::get().to(graph::index)),
            )
            .service(
                actix_web::web::resource(&format!("{}/graph", app_prefix.clone()))
                    .route(actix_web::web::get().to(graph::index)),
            )
    })
    .keep_alive(Duration::new(10, 0))
    .bind(service_addr)?
    .run();

    // Public service.
    let public_state = state;
    let public_server = HttpServer::new(move || {
        App::new()
            .wrap(middleware::Compress::default())
            .wrap_fn(|req, srv| {
                let parent_context = get_context(&req);
                let mut span = get_tracer().start_with_context("request", parent_context);
                set_span_tags(req.path(), req.headers(), &mut span);
                let _active_span = mark_span_as_active(span);
                let cx = ot_context::current();
                srv.call(req).with_context(cx)
            })
            .app_data(actix_web::web::Data::new(public_state.clone()))
            .service(
                actix_web::web::resource(&format!("{}/graph-data", public_app_prefix.clone()))
                    .route(actix_web::web::get().to(graph::graph_data)),
            )
    })
    .keep_alive(Duration::new(10, 0))
    .bind(public_addr)?
    .run();

    future::try_join3(metrics_server, main_server, public_server).await?;

    Ok(())
}

fn ensure_registered_metrics(
    registry: &prometheus::Registry,
    metrics_prefix: &str,
    metrics_required: &HashSet<String>,
) -> Fallible<()> {
    let registered_metric_names = registry
        .gather()
        .iter()
        .map(prometheus::proto::MetricFamily::get_name)
        .map(Into::into)
        .collect::<HashSet<String>>();

    metrics_required.iter().try_for_each(|required_metric| {
        ensure!(
            registered_metric_names.contains(&format!("{}_{}", metrics_prefix, required_metric)),
            "Required metric '{}' has not been registered: {:#?}",
            required_metric,
            registered_metric_names,
        );

        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::graph_builder::graph::{self, State};
    use actix_web::body::MessageBody;
    use commons::metrics::HasRegistry;
    use commons::metrics::RegistryWrapper;
    use commons::testing;
    use graph_builder::status::{serve_liveness, serve_readiness};
    use memchr::memmem;
    use parking_lot::RwLock;
    use prometheus::Registry;
    use std::collections::HashSet;
    use std::sync::Arc;

    fn mock_state(is_live: bool, is_ready: bool) -> State {
        let json_graph = Arc::new(RwLock::new(String::new()));
        let live = Arc::new(RwLock::new(is_live));
        let ready = Arc::new(RwLock::new(is_ready));

        let plugins = Box::leak(Box::new([]));
        let registry: &'static Registry = Box::leak(Box::new(
            metrics::new_registry(Some(config::METRICS_PREFIX.to_string())).unwrap(),
        ));
        let secondary_metadata = Arc::new(RwLock::new(String::new()));

        State::new(
            json_graph,
            HashSet::new(),
            live,
            ready,
            plugins,
            registry,
            secondary_metadata,
        )
    }

    #[test]
    fn serve_metrics_basic() -> Fallible<()> {
        let rt = testing::init_runtime()?;
        let state = mock_state(false, false);

        let registry = <dyn HasRegistry>::registry(&state);
        graph::register_metrics(registry)?;
        testing::dummy_gauge(registry, 42.0)?;

        let metrics_call =
            metrics::serve::<RegistryWrapper>(actix_web::web::Data::new(RegistryWrapper(registry)));
        let resp = rt.block_on(metrics_call);

        assert_eq!(resp.status(), 200);
        if let Ok(bytes) = resp.into_body().try_into_bytes() {
            assert!(!bytes.is_empty());
            println!("{:?}", std::str::from_utf8(bytes.as_ref()));
            assert!(
                memmem::find_iter(bytes.as_ref(), b"cincinnati_gb_dummy_gauge 42\n")
                    .next()
                    .is_some()
            );
        } else {
            bail!("expected bytes in body")
        };

        Ok(())
    }

    #[test]
    fn check_liveness_readiness() -> Fallible<()> {
        let rt = testing::init_runtime()?;

        let liveness_is_live = serve_liveness(actix_web::web::Data::new(mock_state(true, false)));
        let resp = rt.block_on(liveness_is_live);
        assert!(
            resp.status().is_success(),
            "liveness check failed. Application returned {}, expected success",
            resp.status()
        );

        let liveness_not_live = serve_liveness(actix_web::web::Data::new(mock_state(false, false)));
        let resp = rt.block_on(liveness_not_live);
        assert!(
            !resp.status().is_success(),
            "liveness check failed. Application returned {}, expected failure",
            resp.status()
        );

        let readiness_is_ready = serve_readiness(actix_web::web::Data::new(mock_state(true, true)));
        let resp = rt.block_on(readiness_is_ready);
        assert!(
            resp.status().is_success(),
            "readiness check failed. Application returned {}, expected success",
            resp.status()
        );

        let readiness_not_ready =
            serve_readiness(actix_web::web::Data::new(mock_state(true, false)));
        let resp = rt.block_on(readiness_not_ready);
        assert!(
            !resp.status().is_success(),
            "readiness check failed. Application returned {}, expected failure",
            resp.status()
        );

        Ok(())
    }
}
