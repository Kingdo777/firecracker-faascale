// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use micro_http::StatusCode;
use vmm::vmm_config::faascale_mem::{
    FaascaleMemDeviceConfig,  FaascaleMemUpdateStatsConfig,
};

use super::super::VmmAction;
use crate::parsed_request::{Error, ParsedRequest};
use crate::request::Body;

pub(crate) fn parse_get_faascale_mem(path_second_token: Option<&&str>) -> Result<ParsedRequest, Error> {
    match path_second_token {
        Some(stats_path) => match *stats_path {
            "statistics" => Ok(ParsedRequest::new_sync(VmmAction::GetFaascaleMemStats)),
            _ => Err(Error::Generic(
                StatusCode::BadRequest,
                format!("Unrecognized GET request path `{}`.", *stats_path),
            )),
        },
        None => Ok(ParsedRequest::new_sync(VmmAction::GetFaascaleMemConfig)),
    }
}

pub(crate) fn parse_put_faascale_mem(body: &Body) -> Result<ParsedRequest, Error> {
    Ok(ParsedRequest::new_sync(VmmAction::SetFaascaleMemDevice(
        serde_json::from_slice::<FaascaleMemDeviceConfig>(body.raw())?,
    )))
}

pub(crate) fn parse_patch_faascale_mem(
    body: &Body,
    path_second_token: Option<&&str>,
) -> Result<ParsedRequest, Error> {
    match path_second_token {
        Some(config_path) => match *config_path {
            "statistics" => Ok(ParsedRequest::new_sync(VmmAction::UpdateFaascaleMemStatistics(
                serde_json::from_slice::<FaascaleMemUpdateStatsConfig>(body.raw())?,
            ))),
            _ => Err(Error::Generic(
                StatusCode::BadRequest,
                format!("Unrecognized PATCH request path `{}`.", *config_path),
            )),
        },
        None => Err(Error::Generic(
            StatusCode::BadRequest,
            format!("Unrecognized PATCH request path, We haven't support update size."),
        )),
    }
}