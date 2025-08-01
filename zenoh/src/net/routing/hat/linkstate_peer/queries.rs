//
// Copyright (c) 2023 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//
use std::{
    borrow::Cow,
    collections::HashMap,
    sync::{atomic::Ordering, Arc},
};

use petgraph::graph::NodeIndex;
use zenoh_protocol::{
    core::{
        key_expr::{
            include::{Includer, DEFAULT_INCLUDER},
            OwnedKeyExpr,
        },
        WhatAmI, ZenohIdProto,
    },
    network::{
        declare::{
            common::ext::WireExprType, ext, queryable::ext::QueryableInfoType, Declare,
            DeclareBody, DeclareQueryable, QueryableId, UndeclareQueryable,
        },
        interest::{InterestId, InterestMode},
    },
};
use zenoh_sync::get_mut_unchecked;

use super::{
    face_hat, face_hat_mut, get_peer, hat, hat_mut, push_declaration_profile, res_hat, res_hat_mut,
    HatCode, HatContext, HatFace, HatTables,
};
use crate::{
    key_expr::KeyExpr,
    net::{
        protocol::network::Network,
        routing::{
            dispatcher::{
                face::FaceState,
                resource::{NodeId, Resource, SessionContext},
                tables::{QueryTargetQabl, QueryTargetQablSet, RoutingExpr, Tables},
            },
            hat::{CurrentFutureTrait, HatQueriesTrait, SendDeclare, Sources},
            router::disable_matches_query_routes,
            RoutingContext,
        },
    },
};

#[inline]
fn merge_qabl_infos(mut this: QueryableInfoType, info: &QueryableInfoType) -> QueryableInfoType {
    this.complete = this.complete || info.complete;
    this.distance = std::cmp::min(this.distance, info.distance);
    this
}

fn local_peer_qabl_info(_tables: &Tables, res: &Arc<Resource>) -> QueryableInfoType {
    res.session_ctxs
        .values()
        .fold(None, |accu, ctx| {
            if let Some(info) = ctx.qabl.as_ref() {
                Some(match accu {
                    Some(accu) => merge_qabl_infos(accu, info),
                    None => *info,
                })
            } else {
                accu
            }
        })
        .unwrap_or(QueryableInfoType::DEFAULT)
}

fn local_qabl_info(
    tables: &Tables,
    res: &Arc<Resource>,
    face: &Arc<FaceState>,
) -> QueryableInfoType {
    let info = if res.context.is_some() {
        res_hat!(res)
            .linkstatepeer_qabls
            .iter()
            .fold(None, |accu, (zid, info)| {
                if *zid != tables.zid {
                    Some(match accu {
                        Some(accu) => merge_qabl_infos(accu, info),
                        None => *info,
                    })
                } else {
                    accu
                }
            })
    } else {
        None
    };
    res.session_ctxs
        .values()
        .fold(info, |accu, ctx| {
            if ctx.face.id != face.id && ctx.face.whatami != WhatAmI::Peer
                || face.whatami != WhatAmI::Peer
            {
                if let Some(info) = ctx.qabl.as_ref() {
                    Some(match accu {
                        Some(accu) => merge_qabl_infos(accu, info),
                        None => *info,
                    })
                } else {
                    accu
                }
            } else {
                accu
            }
        })
        .unwrap_or(QueryableInfoType::DEFAULT)
}

#[inline]
fn send_sourced_queryable_to_net_children(
    tables: &Tables,
    net: &Network,
    children: &[NodeIndex],
    res: &Arc<Resource>,
    qabl_info: &QueryableInfoType,
    src_face: Option<&mut Arc<FaceState>>,
    routing_context: NodeId,
) {
    for child in children {
        if net.graph.contains_node(*child) {
            match tables.get_face(&net.graph[*child].zid).cloned() {
                Some(mut someface) => {
                    if src_face
                        .as_ref()
                        .map(|src_face| someface.id != src_face.id)
                        .unwrap_or(true)
                    {
                        let push_declaration = push_declaration_profile(&someface);
                        let key_expr = Resource::decl_key(res, &mut someface, push_declaration);

                        someface.primitives.send_declare(RoutingContext::with_expr(
                            &mut Declare {
                                interest_id: None,
                                ext_qos: ext::QoSType::DECLARE,
                                ext_tstamp: None,
                                ext_nodeid: ext::NodeIdType {
                                    node_id: routing_context,
                                },
                                body: DeclareBody::DeclareQueryable(DeclareQueryable {
                                    id: 0, // Sourced queryables do not use ids
                                    wire_expr: key_expr,
                                    ext_info: *qabl_info,
                                }),
                            },
                            res.expr().to_string(),
                        ));
                    }
                }
                None => tracing::trace!("Unable to find face for zid {}", net.graph[*child].zid),
            }
        }
    }
}

fn propagate_simple_queryable(
    tables: &mut Tables,
    res: &Arc<Resource>,
    src_face: Option<&mut Arc<FaceState>>,
    send_declare: &mut SendDeclare,
) {
    let faces = tables.faces.values().cloned();
    for mut dst_face in faces {
        let info = local_qabl_info(tables, res, &dst_face);
        let current = face_hat!(dst_face).local_qabls.get(res);
        if src_face
            .as_ref()
            .map(|src_face| dst_face.id != src_face.id)
            .unwrap_or(true)
            && (current.is_none() || current.unwrap().1 != info)
            && dst_face.whatami == WhatAmI::Client
            && face_hat!(dst_face)
                .remote_interests
                .values()
                .any(|i| i.options.queryables() && i.matches(res))
        {
            let id = current
                .map(|c| c.0)
                .unwrap_or(face_hat!(dst_face).next_id.fetch_add(1, Ordering::SeqCst));
            face_hat_mut!(&mut dst_face)
                .local_qabls
                .insert(res.clone(), (id, info));
            let push_declaration = push_declaration_profile(&dst_face);
            let key_expr = Resource::decl_key(res, &mut dst_face, push_declaration);
            send_declare(
                &dst_face.primitives,
                RoutingContext::with_expr(
                    Declare {
                        interest_id: None,
                        ext_qos: ext::QoSType::DECLARE,
                        ext_tstamp: None,
                        ext_nodeid: ext::NodeIdType::DEFAULT,
                        body: DeclareBody::DeclareQueryable(DeclareQueryable {
                            id,
                            wire_expr: key_expr,
                            ext_info: info,
                        }),
                    },
                    res.expr().to_string(),
                ),
            );
        }
    }
}

fn propagate_sourced_queryable(
    tables: &Tables,
    res: &Arc<Resource>,
    qabl_info: &QueryableInfoType,
    src_face: Option<&mut Arc<FaceState>>,
    source: &ZenohIdProto,
) {
    let net = hat!(tables).linkstatepeers_net.as_ref().unwrap();
    match net.get_idx(source) {
        Some(tree_sid) => {
            if net.trees.len() > tree_sid.index() {
                send_sourced_queryable_to_net_children(
                    tables,
                    net,
                    &net.trees[tree_sid.index()].children,
                    res,
                    qabl_info,
                    src_face,
                    tree_sid.index() as NodeId,
                );
            } else {
                tracing::trace!(
                    "Propagating qabl {}: tree for node {} sid:{} not yet ready",
                    res.expr(),
                    tree_sid.index(),
                    source
                );
            }
        }
        None => tracing::error!(
            "Error propagating qabl {}: cannot get index of {}!",
            res.expr(),
            source
        ),
    }
}

fn register_linkstatepeer_queryable(
    tables: &mut Tables,
    mut face: Option<&mut Arc<FaceState>>,
    res: &mut Arc<Resource>,
    qabl_info: &QueryableInfoType,
    peer: ZenohIdProto,
    send_declare: &mut SendDeclare,
) {
    let current_info = res_hat!(res).linkstatepeer_qabls.get(&peer);
    if current_info.is_none() || current_info.unwrap() != qabl_info {
        // Register peer queryable
        {
            res_hat_mut!(res)
                .linkstatepeer_qabls
                .insert(peer, *qabl_info);
            hat_mut!(tables).linkstatepeer_qabls.insert(res.clone());
        }

        // Propagate queryable to peers
        propagate_sourced_queryable(tables, res, qabl_info, face.as_deref_mut(), &peer);
    }

    // Propagate queryable to clients
    propagate_simple_queryable(tables, res, face, send_declare);
}

fn declare_linkstatepeer_queryable(
    tables: &mut Tables,
    face: &mut Arc<FaceState>,
    res: &mut Arc<Resource>,
    qabl_info: &QueryableInfoType,
    peer: ZenohIdProto,
    send_declare: &mut SendDeclare,
) {
    let face = Some(face);
    register_linkstatepeer_queryable(tables, face, res, qabl_info, peer, send_declare);
}

fn register_simple_queryable(
    _tables: &mut Tables,
    face: &mut Arc<FaceState>,
    id: QueryableId,
    res: &mut Arc<Resource>,
    qabl_info: &QueryableInfoType,
) {
    // Register queryable
    {
        let res = get_mut_unchecked(res);
        get_mut_unchecked(
            res.session_ctxs
                .entry(face.id)
                .or_insert_with(|| Arc::new(SessionContext::new(face.clone()))),
        )
        .qabl = Some(*qabl_info);
    }
    face_hat_mut!(face).remote_qabls.insert(id, res.clone());
}

fn declare_simple_queryable(
    tables: &mut Tables,
    face: &mut Arc<FaceState>,
    id: QueryableId,
    res: &mut Arc<Resource>,
    qabl_info: &QueryableInfoType,
    send_declare: &mut SendDeclare,
) {
    register_simple_queryable(tables, face, id, res, qabl_info);
    let local_details = local_peer_qabl_info(tables, res);
    let zid = tables.zid;
    register_linkstatepeer_queryable(tables, Some(face), res, &local_details, zid, send_declare);
}

#[inline]
fn remote_linkstatepeer_qabls(tables: &Tables, res: &Arc<Resource>) -> bool {
    res.context.is_some()
        && res_hat!(res)
            .linkstatepeer_qabls
            .keys()
            .any(|peer| peer != &tables.zid)
}

#[inline]
fn simple_qabls(res: &Arc<Resource>) -> Vec<Arc<FaceState>> {
    res.session_ctxs
        .values()
        .filter_map(|ctx| {
            if ctx.qabl.is_some() {
                Some(ctx.face.clone())
            } else {
                None
            }
        })
        .collect()
}

#[inline]
fn remote_simple_qabls(res: &Arc<Resource>, face: &Arc<FaceState>) -> bool {
    res.session_ctxs
        .values()
        .any(|ctx| ctx.face.id != face.id && ctx.qabl.is_some())
}

#[inline]
fn send_forget_sourced_queryable_to_net_children(
    tables: &Tables,
    net: &Network,
    children: &[NodeIndex],
    res: &Arc<Resource>,
    src_face: Option<&Arc<FaceState>>,
    routing_context: NodeId,
) {
    for child in children {
        if net.graph.contains_node(*child) {
            match tables.get_face(&net.graph[*child].zid).cloned() {
                Some(mut someface) => {
                    if src_face
                        .map(|src_face| someface.id != src_face.id)
                        .unwrap_or(true)
                    {
                        let push_declaration = push_declaration_profile(&someface);
                        let wire_expr = Resource::decl_key(res, &mut someface, push_declaration);

                        someface.primitives.send_declare(RoutingContext::with_expr(
                            &mut Declare {
                                interest_id: None,
                                ext_qos: ext::QoSType::DECLARE,
                                ext_tstamp: None,
                                ext_nodeid: ext::NodeIdType {
                                    node_id: routing_context,
                                },
                                body: DeclareBody::UndeclareQueryable(UndeclareQueryable {
                                    id: 0,
                                    ext_wire_expr: WireExprType { wire_expr },
                                }),
                            },
                            res.expr().to_string(),
                        ));
                    }
                }
                None => tracing::trace!("Unable to find face for zid {}", net.graph[*child].zid),
            }
        }
    }
}

fn propagate_forget_simple_queryable(
    tables: &mut Tables,
    res: &mut Arc<Resource>,
    send_declare: &mut SendDeclare,
) {
    for mut face in tables.faces.values().cloned() {
        if let Some((id, _)) = face_hat_mut!(&mut face).local_qabls.remove(res) {
            send_declare(
                &face.primitives,
                RoutingContext::with_expr(
                    Declare {
                        interest_id: None,
                        ext_qos: ext::QoSType::DECLARE,
                        ext_tstamp: None,
                        ext_nodeid: ext::NodeIdType::DEFAULT,
                        body: DeclareBody::UndeclareQueryable(UndeclareQueryable {
                            id,
                            ext_wire_expr: WireExprType::null(),
                        }),
                    },
                    res.expr().to_string(),
                ),
            );
        }
        for res in face_hat!(&mut face)
            .local_qabls
            .keys()
            .cloned()
            .collect::<Vec<Arc<Resource>>>()
        {
            if !res.context().matches.iter().any(|m| {
                m.upgrade().is_some_and(|m| {
                    m.context.is_some()
                        && (remote_simple_qabls(&m, &face)
                            || remote_linkstatepeer_qabls(tables, &m))
                })
            }) {
                if let Some((id, _)) = face_hat_mut!(&mut face).local_qabls.remove(&res) {
                    send_declare(
                        &face.primitives,
                        RoutingContext::with_expr(
                            Declare {
                                interest_id: None,
                                ext_qos: ext::QoSType::DECLARE,
                                ext_tstamp: None,
                                ext_nodeid: ext::NodeIdType::DEFAULT,
                                body: DeclareBody::UndeclareQueryable(UndeclareQueryable {
                                    id,
                                    ext_wire_expr: WireExprType::null(),
                                }),
                            },
                            res.expr().to_string(),
                        ),
                    );
                }
            }
        }
    }
}

fn propagate_forget_sourced_queryable(
    tables: &mut Tables,
    res: &mut Arc<Resource>,
    src_face: Option<&Arc<FaceState>>,
    source: &ZenohIdProto,
) {
    let net = hat!(tables).linkstatepeers_net.as_ref().unwrap();
    match net.get_idx(source) {
        Some(tree_sid) => {
            if net.trees.len() > tree_sid.index() {
                send_forget_sourced_queryable_to_net_children(
                    tables,
                    net,
                    &net.trees[tree_sid.index()].children,
                    res,
                    src_face,
                    tree_sid.index() as NodeId,
                );
            } else {
                tracing::trace!(
                    "Propagating forget qabl {}: tree for node {} sid:{} not yet ready",
                    res.expr(),
                    tree_sid.index(),
                    source
                );
            }
        }
        None => tracing::error!(
            "Error propagating forget qabl {}: cannot get index of {}!",
            res.expr(),
            source
        ),
    }
}

fn unregister_linkstatepeer_queryable(
    tables: &mut Tables,
    res: &mut Arc<Resource>,
    peer: &ZenohIdProto,
    send_declare: &mut SendDeclare,
) {
    res_hat_mut!(res).linkstatepeer_qabls.remove(peer);

    if res_hat!(res).linkstatepeer_qabls.is_empty() {
        hat_mut!(tables)
            .linkstatepeer_qabls
            .retain(|qabl| !Arc::ptr_eq(qabl, res));

        propagate_forget_simple_queryable(tables, res, send_declare);
    }
}

fn undeclare_linkstatepeer_queryable(
    tables: &mut Tables,
    face: Option<&Arc<FaceState>>,
    res: &mut Arc<Resource>,
    peer: &ZenohIdProto,
    send_declare: &mut SendDeclare,
) {
    if res_hat!(res).linkstatepeer_qabls.contains_key(peer) {
        unregister_linkstatepeer_queryable(tables, res, peer, send_declare);
        propagate_forget_sourced_queryable(tables, res, face, peer);
    }
}

fn forget_linkstatepeer_queryable(
    tables: &mut Tables,
    face: &mut Arc<FaceState>,
    res: &mut Arc<Resource>,
    peer: &ZenohIdProto,
    send_declare: &mut SendDeclare,
) {
    undeclare_linkstatepeer_queryable(tables, Some(face), res, peer, send_declare);
}

pub(super) fn undeclare_simple_queryable(
    tables: &mut Tables,
    face: &mut Arc<FaceState>,
    res: &mut Arc<Resource>,
    send_declare: &mut SendDeclare,
) {
    if !face_hat_mut!(face)
        .remote_qabls
        .values()
        .any(|s| *s == *res)
    {
        if let Some(ctx) = get_mut_unchecked(res).session_ctxs.get_mut(&face.id) {
            get_mut_unchecked(ctx).qabl = None;
        }

        let mut simple_qabls = simple_qabls(res);
        let linkstatepeer_qabls = remote_linkstatepeer_qabls(tables, res);

        if simple_qabls.is_empty() {
            undeclare_linkstatepeer_queryable(tables, None, res, &tables.zid.clone(), send_declare);
        } else {
            let local_info = local_peer_qabl_info(tables, res);
            register_linkstatepeer_queryable(
                tables,
                None,
                res,
                &local_info,
                tables.zid,
                send_declare,
            );
        }

        if simple_qabls.len() == 1 && !linkstatepeer_qabls {
            let mut face = &mut simple_qabls[0];
            if let Some((id, _)) = face_hat_mut!(face).local_qabls.remove(res) {
                send_declare(
                    &face.primitives,
                    RoutingContext::with_expr(
                        Declare {
                            interest_id: None,
                            ext_qos: ext::QoSType::DECLARE,
                            ext_tstamp: None,
                            ext_nodeid: ext::NodeIdType::DEFAULT,
                            body: DeclareBody::UndeclareQueryable(UndeclareQueryable {
                                id,
                                ext_wire_expr: WireExprType::null(),
                            }),
                        },
                        res.expr().to_string(),
                    ),
                );
            }
            for res in face_hat!(face)
                .local_qabls
                .keys()
                .cloned()
                .collect::<Vec<Arc<Resource>>>()
            {
                if !res.context().matches.iter().any(|m| {
                    m.upgrade().is_some_and(|m| {
                        m.context.is_some()
                            && (remote_simple_qabls(&m, face)
                                || remote_linkstatepeer_qabls(tables, &m))
                    })
                }) {
                    if let Some((id, _)) = face_hat_mut!(&mut face).local_qabls.remove(&res) {
                        send_declare(
                            &face.primitives,
                            RoutingContext::with_expr(
                                Declare {
                                    interest_id: None,
                                    ext_qos: ext::QoSType::DECLARE,
                                    ext_tstamp: None,
                                    ext_nodeid: ext::NodeIdType::DEFAULT,
                                    body: DeclareBody::UndeclareQueryable(UndeclareQueryable {
                                        id,
                                        ext_wire_expr: WireExprType::null(),
                                    }),
                                },
                                res.expr().to_string(),
                            ),
                        );
                    }
                }
            }
        }
    }
}

fn forget_simple_queryable(
    tables: &mut Tables,
    face: &mut Arc<FaceState>,
    id: QueryableId,
    send_declare: &mut SendDeclare,
) -> Option<Arc<Resource>> {
    if let Some(mut res) = face_hat_mut!(face).remote_qabls.remove(&id) {
        undeclare_simple_queryable(tables, face, &mut res, send_declare);
        Some(res)
    } else {
        None
    }
}

pub(super) fn queries_remove_node(
    tables: &mut Tables,
    node: &ZenohIdProto,
    send_declare: &mut SendDeclare,
) {
    let mut qabls = vec![];
    for res in hat!(tables).linkstatepeer_qabls.iter() {
        for qabl in res_hat!(res).linkstatepeer_qabls.keys() {
            if qabl == node {
                qabls.push(res.clone());
            }
        }
    }
    for mut res in qabls {
        unregister_linkstatepeer_queryable(tables, &mut res, node, send_declare);

        disable_matches_query_routes(tables, &mut res);
        Resource::clean(&mut res)
    }
}

pub(super) fn queries_tree_change(tables: &mut Tables, new_children: &[Vec<NodeIndex>]) {
    let net = match hat!(tables).linkstatepeers_net.as_ref() {
        Some(net) => net,
        None => {
            tracing::error!("Error accessing peers_net in queries_tree_change!");
            return;
        }
    };
    // propagate qabls to new clildren
    for (tree_sid, tree_children) in new_children.iter().enumerate() {
        if !tree_children.is_empty() {
            let tree_idx = NodeIndex::new(tree_sid);
            if net.graph.contains_node(tree_idx) {
                let tree_id = net.graph[tree_idx].zid;

                let qabls_res = &hat!(tables).linkstatepeer_qabls;

                for res in qabls_res {
                    let qabls = &res_hat!(res).linkstatepeer_qabls;
                    if let Some(qabl_info) = qabls.get(&tree_id) {
                        send_sourced_queryable_to_net_children(
                            tables,
                            net,
                            tree_children,
                            res,
                            qabl_info,
                            None,
                            tree_sid as NodeId,
                        );
                    }
                }
            }
        }
    }
}

#[inline]
fn insert_target_for_qabls(
    route: &mut QueryTargetQablSet,
    expr: &mut RoutingExpr,
    tables: &Tables,
    net: &Network,
    source: NodeId,
    qabls: &HashMap<ZenohIdProto, QueryableInfoType>,
    complete: bool,
) {
    if net.trees.len() > source as usize {
        for (qabl, qabl_info) in qabls {
            if let Some(qabl_idx) = net.get_idx(qabl) {
                if net.trees[source as usize].directions.len() > qabl_idx.index() {
                    if let Some(direction) = net.trees[source as usize].directions[qabl_idx.index()]
                    {
                        if net.graph.contains_node(direction) {
                            if let Some(face) = tables.get_face(&net.graph[direction].zid) {
                                if net.distances.len() > qabl_idx.index() {
                                    let key_expr =
                                        Resource::get_best_key(expr.prefix, expr.suffix, face.id);
                                    route.push(QueryTargetQabl {
                                        direction: (face.clone(), key_expr.to_owned(), source),
                                        info: Some(QueryableInfoType {
                                            complete: complete && qabl_info.complete,
                                            distance: net.distances[qabl_idx.index()] as u16,
                                        }),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    } else {
        tracing::trace!("Tree for node sid:{} not yet ready", source);
    }
}

lazy_static::lazy_static! {
    static ref EMPTY_ROUTE: Arc<QueryTargetQablSet> = Arc::new(Vec::new());
}

#[inline]
fn make_qabl_id(
    res: &Arc<Resource>,
    face: &mut Arc<FaceState>,
    mode: InterestMode,
    info: QueryableInfoType,
) -> u32 {
    if mode.future() {
        if let Some((id, _)) = face_hat!(face).local_qabls.get(res) {
            *id
        } else {
            let id = face_hat!(face).next_id.fetch_add(1, Ordering::SeqCst);
            face_hat_mut!(face)
                .local_qabls
                .insert(res.clone(), (id, info));
            id
        }
    } else {
        0
    }
}

pub(super) fn declare_qabl_interest(
    tables: &mut Tables,
    face: &mut Arc<FaceState>,
    id: InterestId,
    res: Option<&mut Arc<Resource>>,
    mode: InterestMode,
    aggregate: bool,
    send_declare: &mut SendDeclare,
) {
    if mode.current() && face.whatami == WhatAmI::Client {
        let interest_id = Some(id);
        if let Some(res) = res.as_ref() {
            if aggregate {
                if hat!(tables).linkstatepeer_qabls.iter().any(|qabl| {
                    qabl.context.is_some()
                        && qabl.matches(res)
                        && (remote_simple_qabls(qabl, face)
                            || remote_linkstatepeer_qabls(tables, qabl))
                }) {
                    let info = local_qabl_info(tables, res, face);
                    let id = make_qabl_id(res, face, mode, info);
                    let wire_expr = Resource::decl_key(res, face, push_declaration_profile(face));
                    send_declare(
                        &face.primitives,
                        RoutingContext::with_expr(
                            Declare {
                                interest_id,
                                ext_qos: ext::QoSType::DECLARE,
                                ext_tstamp: None,
                                ext_nodeid: ext::NodeIdType::DEFAULT,
                                body: DeclareBody::DeclareQueryable(DeclareQueryable {
                                    id,
                                    wire_expr,
                                    ext_info: info,
                                }),
                            },
                            res.expr().to_string(),
                        ),
                    );
                }
            } else {
                for qabl in hat!(tables).linkstatepeer_qabls.iter() {
                    if qabl.context.is_some()
                        && qabl.matches(res)
                        && (remote_simple_qabls(qabl, face)
                            || remote_linkstatepeer_qabls(tables, qabl))
                    {
                        let info = local_qabl_info(tables, qabl, face);
                        let id = make_qabl_id(qabl, face, mode, info);
                        let key_expr =
                            Resource::decl_key(qabl, face, push_declaration_profile(face));
                        send_declare(
                            &face.primitives,
                            RoutingContext::with_expr(
                                Declare {
                                    interest_id,
                                    ext_qos: ext::QoSType::DECLARE,
                                    ext_tstamp: None,
                                    ext_nodeid: ext::NodeIdType::DEFAULT,
                                    body: DeclareBody::DeclareQueryable(DeclareQueryable {
                                        id,
                                        wire_expr: key_expr,
                                        ext_info: info,
                                    }),
                                },
                                qabl.expr().to_string(),
                            ),
                        );
                    }
                }
            }
        } else {
            for qabl in hat!(tables).linkstatepeer_qabls.iter() {
                if qabl.context.is_some()
                    && (remote_simple_qabls(qabl, face) || remote_linkstatepeer_qabls(tables, qabl))
                {
                    let info = local_qabl_info(tables, qabl, face);
                    let id = make_qabl_id(qabl, face, mode, info);
                    let key_expr = Resource::decl_key(qabl, face, push_declaration_profile(face));
                    send_declare(
                        &face.primitives,
                        RoutingContext::with_expr(
                            Declare {
                                interest_id,
                                ext_qos: ext::QoSType::DECLARE,
                                ext_tstamp: None,
                                ext_nodeid: ext::NodeIdType::DEFAULT,
                                body: DeclareBody::DeclareQueryable(DeclareQueryable {
                                    id,
                                    wire_expr: key_expr,
                                    ext_info: info,
                                }),
                            },
                            qabl.expr().to_string(),
                        ),
                    );
                }
            }
        }
    }
}

impl HatQueriesTrait for HatCode {
    fn declare_queryable(
        &self,
        tables: &mut Tables,
        face: &mut Arc<FaceState>,
        id: QueryableId,
        res: &mut Arc<Resource>,
        qabl_info: &QueryableInfoType,
        node_id: NodeId,
        send_declare: &mut SendDeclare,
    ) {
        if face.whatami != WhatAmI::Client {
            if let Some(peer) = get_peer(tables, face, node_id) {
                declare_linkstatepeer_queryable(tables, face, res, qabl_info, peer, send_declare);
            }
        } else {
            declare_simple_queryable(tables, face, id, res, qabl_info, send_declare);
        }
    }

    fn undeclare_queryable(
        &self,
        tables: &mut Tables,
        face: &mut Arc<FaceState>,
        id: QueryableId,
        res: Option<Arc<Resource>>,
        node_id: NodeId,
        send_declare: &mut SendDeclare,
    ) -> Option<Arc<Resource>> {
        if face.whatami != WhatAmI::Client {
            if let Some(mut res) = res {
                if let Some(peer) = get_peer(tables, face, node_id) {
                    forget_linkstatepeer_queryable(tables, face, &mut res, &peer, send_declare);
                    Some(res)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            forget_simple_queryable(tables, face, id, send_declare)
        }
    }

    fn get_queryables(&self, tables: &Tables) -> Vec<(Arc<Resource>, Sources)> {
        // Compute the list of known queryables (keys)
        hat!(tables)
            .linkstatepeer_qabls
            .iter()
            .map(|s| {
                // Compute the list of routers, peers and clients that are known
                // sources of those queryables
                let mut routers = vec![];
                let mut peers = vec![];
                let mut clients = vec![];
                for q in res_hat!(s).linkstatepeer_qabls.keys() {
                    if let Some(whatami) = hat!(tables)
                        .linkstatepeers_net
                        .as_ref()
                        .and_then(|net| net.get_node(q))
                        .and_then(|n| n.whatami)
                    {
                        match whatami {
                            WhatAmI::Router => routers.push(*q),
                            WhatAmI::Peer => peers.push(*q),
                            WhatAmI::Client => clients.push(*q),
                        }
                    } else {
                        peers.push(*q);
                    }
                }
                for ctx in s.session_ctxs.values().filter(|ctx| {
                    !ctx.face.is_local && ctx.face.whatami == WhatAmI::Client && ctx.qabl.is_some()
                }) {
                    clients.push(ctx.face.zid);
                }
                (
                    s.clone(),
                    Sources {
                        routers,
                        peers,
                        clients,
                    },
                )
            })
            .collect()
    }

    fn get_queriers(&self, tables: &Tables) -> Vec<(Arc<Resource>, Sources)> {
        let mut result = HashMap::new();
        for face in tables.faces.values() {
            for interest in face_hat!(face).remote_interests.values() {
                if interest.options.queryables() {
                    if let Some(res) = interest.res.as_ref() {
                        let sources = result.entry(res.clone()).or_insert_with(Sources::default);
                        let whatami = if face.is_local {
                            tables.whatami
                        } else {
                            face.whatami
                        };
                        match whatami {
                            WhatAmI::Router => sources.routers.push(face.zid),
                            WhatAmI::Peer => sources.peers.push(face.zid),
                            WhatAmI::Client => sources.clients.push(face.zid),
                        }
                    }
                }
            }
        }
        result.into_iter().collect()
    }

    fn compute_query_route(
        &self,
        tables: &Tables,
        expr: &mut RoutingExpr,
        source: NodeId,
        source_type: WhatAmI,
    ) -> Arc<QueryTargetQablSet> {
        let mut route = QueryTargetQablSet::new();
        let key_expr = expr.full_expr();
        if key_expr.ends_with('/') {
            return EMPTY_ROUTE.clone();
        }
        tracing::trace!(
            "compute_query_route({}, {:?}, {:?})",
            key_expr,
            source,
            source_type
        );
        let key_expr = match OwnedKeyExpr::try_from(key_expr) {
            Ok(ke) => ke,
            Err(e) => {
                tracing::warn!("Invalid KE reached the system: {}", e);
                return EMPTY_ROUTE.clone();
            }
        };
        let res = Resource::get_resource(expr.prefix, expr.suffix);
        let matches = res
            .as_ref()
            .and_then(|res| res.context.as_ref())
            .map(|ctx| Cow::from(&ctx.matches))
            .unwrap_or_else(|| Cow::from(Resource::get_matches(tables, &key_expr)));

        for mres in matches.iter() {
            let mres = mres.upgrade().unwrap();
            let complete = DEFAULT_INCLUDER.includes(mres.expr().as_bytes(), key_expr.as_bytes());

            let net = hat!(tables).linkstatepeers_net.as_ref().unwrap();
            let peer_source = match source_type {
                WhatAmI::Router | WhatAmI::Peer => source,
                _ => net.idx.index() as NodeId,
            };
            insert_target_for_qabls(
                &mut route,
                expr,
                tables,
                net,
                peer_source,
                &res_hat!(mres).linkstatepeer_qabls,
                complete,
            );

            for (sid, context) in &mres.session_ctxs {
                if source_type == WhatAmI::Client || context.face.whatami == WhatAmI::Client {
                    let key_expr = Resource::get_best_key(expr.prefix, expr.suffix, *sid);
                    if let Some(qabl_info) = context.qabl.as_ref() {
                        route.push(QueryTargetQabl {
                            direction: (
                                context.face.clone(),
                                key_expr.to_owned(),
                                NodeId::default(),
                            ),
                            info: Some(QueryableInfoType {
                                complete: complete && qabl_info.complete,
                                distance: 1,
                            }),
                        });
                    }
                }
            }
        }
        route.sort_by_key(|qabl| qabl.info.map_or(u16::MAX, |i| i.distance));
        Arc::new(route)
    }

    fn get_matching_queryables(
        &self,
        tables: &Tables,
        key_expr: &KeyExpr<'_>,
        complete: bool,
    ) -> HashMap<usize, Arc<FaceState>> {
        let mut matching_queryables = HashMap::new();
        if key_expr.ends_with('/') {
            return matching_queryables;
        }
        tracing::trace!(
            "get_matching_queryables({}; complete: {})",
            key_expr,
            complete
        );

        let res = Resource::get_resource(&tables.root_res, key_expr);
        let matches = res
            .as_ref()
            .and_then(|res| res.context.as_ref())
            .map(|ctx| Cow::from(&ctx.matches))
            .unwrap_or_else(|| Cow::from(Resource::get_matches(tables, key_expr)));

        for mres in matches.iter() {
            let mres = mres.upgrade().unwrap();
            if complete && !KeyExpr::keyexpr_include(mres.expr(), key_expr) {
                continue;
            }

            let net = hat!(tables).linkstatepeers_net.as_ref().unwrap();
            insert_faces_for_qbls(
                &mut matching_queryables,
                tables,
                net,
                &res_hat!(mres).linkstatepeer_qabls,
                complete,
            );

            for (sid, context) in &mres.session_ctxs {
                if match complete {
                    true => context.qabl.is_some_and(|q| q.complete),
                    false => context.qabl.is_some(),
                } {
                    matching_queryables
                        .entry(*sid)
                        .or_insert_with(|| context.face.clone());
                }
            }
        }
        matching_queryables
    }
}

#[inline]
fn insert_faces_for_qbls(
    route: &mut HashMap<usize, Arc<FaceState>>,
    tables: &Tables,
    net: &Network,
    qbls: &HashMap<ZenohIdProto, QueryableInfoType>,
    complete: bool,
) {
    let source = net.idx.index();
    if net.trees.len() > source {
        for qbl in qbls {
            if complete && !qbl.1.complete {
                continue;
            }
            if let Some(qbl_idx) = net.get_idx(qbl.0) {
                if net.trees[source].directions.len() > qbl_idx.index() {
                    if let Some(direction) = net.trees[source].directions[qbl_idx.index()] {
                        if net.graph.contains_node(direction) {
                            if let Some(face) = tables.get_face(&net.graph[direction].zid) {
                                route.entry(face.id).or_insert_with(|| face.clone());
                            }
                        }
                    }
                }
            }
        }
    } else {
        tracing::trace!("Tree for node sid:{} not yet ready", source);
    }
}
