 //
// Copyright (c) 2017, 2020 ADLINK Technology Inc.
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ADLINK zenoh team, <zenoh@adlink-labs.tech>
//
#![recursion_limit="256"]

use log::{debug, info};
use std::collections::HashMap;
use futures::prelude::*;
use futures::select;
use clap::{Arg, ArgMatches};
use zenoh_router::runtime::Runtime;
use zenoh::net::*;
use zenoh::net::queryable::STORAGE;


#[no_mangle]
pub fn get_expected_args<'a, 'b>() -> Vec<Arg<'a, 'b>>
{
    vec![
        Arg::from_usage("--storage-selector 'The selection of resources to be stored'")
        .default_value("/demo/example/**")
    ]
}

#[no_mangle]
pub fn start(runtime: Runtime, args: &'static ArgMatches<'_>)
{
    async_std::task::spawn(run(runtime, args));
}

async fn run(runtime: Runtime, args: &'static ArgMatches<'_>) {
    env_logger::init();

    let session = Session::init(runtime).await;

    let mut stored: HashMap<String, (RBuf, Option<RBuf>)> = HashMap::new();

    let sub_info = SubInfo {
        reliability: Reliability::Reliable,
        mode: SubMode::Push,
        period: None
    };

    let selector: ResKey = args.value_of("storage-selector").unwrap().into();
    debug!("Run example-plugin with storage-selector={}", selector);

    debug!("Declaring Subscriber on {}", selector);
    let mut sub = session.declare_subscriber(&selector, &sub_info).await.unwrap();

    debug!("Declaring Queryable on {}", selector);
    let mut queryable = session.declare_queryable(&selector, STORAGE).await.unwrap();
    
    loop {
        select!(
            sample = sub.next().fuse() => {
                let sample = sample.unwrap();
                info!("Received data ('{}': '{}')", sample.res_name, sample.payload);
                stored.insert(sample.res_name.into(), (sample.payload, sample.data_info));
            },

            query = queryable.next().fuse() => {
                let query = query.unwrap();
                info!("Handling query '{}?{}'", query.res_name, query.predicate);
                for (rname, (data, data_info)) in stored.iter() {
                    if rname_intersect(&query.res_name, rname) {
                        query.replies_sender.send(Sample{
                            res_name: rname.clone(), 
                            payload: data.clone(), 
                            data_info: data_info.clone(),
                        }).await;
                    }
                }
            }
        );
    }
}
