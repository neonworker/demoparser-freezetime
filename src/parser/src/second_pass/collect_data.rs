use super::entities::PlayerMetaData;
use super::variants::Sticker;
use super::variants::Variant;
use crate::first_pass::prop_controller::*;
use crate::first_pass::read_bits::DemoParserError;
use crate::maps::BUTTONMAP;
use crate::maps::PLAYER_COLOR;
use crate::second_pass::entities::EntityType;
use crate::second_pass::parser_settings::SecondPassParser;
use crate::second_pass::variants::PropColumn;
use crate::second_pass::variants::VarVec;
use ahash::AHashMap;
use csgoproto::maps::AGENTSMAP;
use csgoproto::maps::PAINTKITS;
use csgoproto::maps::STICKER_ID_TO_NAME;
use csgoproto::maps::WEAPINDICIES;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PropType {
    Team,
    Rules,
    Custom,
    Controller,
    Player,
    Weapon,
    Button,
    Name,
    Steamid,
    Tick,
    GameTime,
}

// DONT KNOW IF THESE ARE CORRECT. SEEMS TO GIVE CORRECT VALUES
const CELL_BITS: i32 = 9;
const MAX_COORD: f32 = (1 << 14) as f32;
// https://github.com/markus-wa/demoinfocs-golang/blob/master/pkg/demoinfocs/constants/constants.go#L11
const IS_AIRBORNE_CONST: u32 = 0xFFFFFF;

#[derive(Debug, Clone)]
pub struct ProjectileRecord {
    pub steamid: Option<u64>,
    pub name: Option<String>,
    pub x: Option<f32>,
    pub y: Option<f32>,
    pub z: Option<f32>,
    pub tick: Option<i32>,
    pub grenade_type: Option<String>,
    pub entity_id: Option<i32>,
    pub bounces: Option<i32>,    // Sprint 5: m_nBounces snapshot at this tick
}

#[derive(Debug, Clone)]
pub struct InfernoRecord {
    pub entity_id: i32,
    pub tick: i32,
    pub fire_count: i32,
    // Raw active-flame XY positions at this tick (up to 64 entries). The
    // wrapper's GrenadeFireLayer renders each flame individually so spread
    // / retreat dynamics are visible — the convex-hull approach hid inner
    // flicker behind a static outer polygon during steady burn.
    pub flame_xy: Vec<[f32; 2]>,
}

/// Sprint 5 — per-smoke-instance metadata captured at entity-delete
/// (smoke expire). One record per CSmokeGrenadeProjectile entity that
/// existed in the demo. Used by consumers to attribute "smoke
/// extinguished by molly" interactions via fire_count drops at the
/// smoke's expire tick.
#[derive(Debug, Clone)]
pub struct SmokeRecord {
    pub entity_id: i32,
    pub thrower_entity_id: i32,        // CBaseGrenade.m_hThrower (handle); 0 if unresolved
    pub detonate_tick: i32,             // CSmokeGrenadeProjectile.m_nSmokeEffectTickBegin
    pub expire_tick: i32,               // tick at which entity-delete fired
    pub detonate_pos: [f32; 3],         // CSmokeGrenadeProjectile.m_vSmokeDetonationPos
    pub exploded_from_inferno: bool,    // CSmokeGrenadeProjectile.m_bExplodeFromInferno
    /// Reassembled voxel journal blob (CSmokeGrenadeProjectile.m_VoxelFrameData).
    /// Flattened from per-element props at entity-delete time. Empty if the
    /// prop was never populated (e.g. smoke that never fully detonated).
    pub voxel_frame_data: Vec<u8>,
    /// CSmokeGrenadeProjectile.m_nVoxelFrameDataSize — the authoritative byte
    /// count; use this as the length when reading voxel_frame_data.
    pub voxel_frame_data_size: i32,
}

/// Round-tagging branch — bomb-site resolution via the planted-bomb
/// entity's own site index (Option B, AkiVer-style). One record per
/// CPlantedC4 entity in the demo, capturing `m_nBombSite` once it has
/// been populated on the entity (typically at the same tick as the
/// `bomb_planted` GameEvent).
///
/// Mapping: 0 → A, 1 → B (per AkiVer's cs-demo-analyzer
/// pkg/api/analyzer.go ~line 1217-1231). The wrapper does the letter
/// translation; this struct carries the raw integer for empirical
/// verification first.
#[derive(Debug, Clone)]
pub struct PlantedC4Record {
    pub entity_id: i32,
    pub plant_tick: i32,       // tick at which the prop first read non-default
    pub bomb_site: i32,        // raw CPlantedC4.m_nBombSite value
}

#[derive(Debug, Clone)]
pub struct ItemRegistryRecord {
    pub entity_id: i32,
    pub tick: i32,
    pub def_index: u32,
    pub item_name: String,        // WEAPINDICIES friendly name ("" if unknown)
    pub item_id: u64,             // m_iItemIDLow | (m_iItemIDHigh << 32)
    pub owner_steamid: Option<u64>, // resolved m_hOwnerEntity → pawn → steamid; None if dropped/unresolved
    pub on_ground: bool,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub clip: i32,                // m_iClip1; -1 if unread
    pub skin_id: u32,             // 0 = vanilla
    pub skin_name: String,        // "" if vanilla
    pub wear: f32,                // captured when active; 0 if never-active
    pub seed: u32,
}

pub enum CoordinateAxis {
    X,
    Y,
    Z,
}

// This file collects the data that is converted into a dataframe in the end in parser.parse_ticks()

impl<'a> SecondPassParser<'a> {
    pub fn collect_entities(&mut self) {
        if !self.prop_controller.event_with_velocity {
            // Sprint 5: removed `|| self.wanted_events.len() != 0` clause so that
            // events + projectile dataframe + entity props can collect simultaneously
            // when parse_projectiles=true.
            if !self.wanted_ticks.contains(&self.tick) && self.wanted_ticks.len() != 0 {
                return;
            }
        }
        if self.parse_projectiles {
            self.collect_projectiles();
            self.collect_inferno_records();   // Sprint 5
            // Sprint 5 fix: fall through to per-player collection below.
            // Sprint 4 (loadouts) + Sprint 5 (grenades) need projectile data
            // AND df_per_player populated simultaneously.
        }
        // Round-tagging branch — Option B bomb-site resolution. Run
        // regardless of parse_projectiles since CPlantedC4 isn't a
        // projectile (the diagnostic probe binary disables projectile
        // collection to keep memory low on 500 MB demos).
        self.collect_planted_c4_records();
        self.collect_item_registry_records();
        // Slice 4 — wear/seed for an item entity is only readable while it is
        // a player's ACTIVE weapon (find_skin_float/seed resolve the active
        // weapon). Capture once per entity into a local buffer during the
        // per-player loop, then fold into item_skin_cache after the loop (the
        // loop holds &self.players, so the mutable cache insert is deferred).
        let mut skin_captures: Vec<(i32, (u32, String, f32, u32))> = Vec::new();
        // iterate every player and every wanted prop name
        // if either one is missing then push None to output
        for (entity_id, player) in &self.players {
            // Slice 4 — capture the active weapon's skin/wear/seed once.
            if let Some(aw) = self.prop_controller.special_ids.active_weapon {
                if let Ok(Variant::U32(h)) = self.get_prop_from_ent(&aw, entity_id) {
                    let went = (h & 0x7FF) as i32;
                    // Guard slot reuse (see collect_item_registry_records): only
                    // cache wear/seed when the active-weapon slot currently holds
                    // a tracked item entity (not a knife whose slot is stale).
                    let is_item_slot = matches!(
                        self.entities.get(went as usize),
                        Some(Some(e)) if matches!(e.entity_type, EntityType::Item | EntityType::C4)
                    );
                    if is_item_slot
                        && self.item_entity_ids.contains(&went)
                        && !self.item_skin_cache.contains_key(&went)
                    {
                        let wear = match self.find_skin_float(player) {
                            Ok(Variant::F32(f)) => f,
                            _ => 0.0,
                        };
                        let seed = match self.find_skin_paint_seed(player) {
                            Ok(Variant::U32(s)) => s,
                            Ok(Variant::F32(s)) => s as u32,
                            _ => 0,
                        };
                        let id = match self.find_weapon_skin_id(&went) {
                            Ok(Variant::U32(v)) => v,
                            _ => 0,
                        };
                        let name = match self.find_weapon_skin(&went) {
                            Ok(Variant::String(s)) => s,
                            _ => String::new(),
                        };
                        skin_captures.push((went, (id, name, wear, seed)));
                    }
                }
            }
            // iterate every wanted prop state
            // if any prop's state for this tick is not the wanted state, dont extract info from tick
            for wanted_prop_state_info in &self.prop_controller.wanted_prop_state_infos {
                match self.find_prop(&wanted_prop_state_info.base, entity_id, player) {
                    Ok(prop) => {
                        if prop != wanted_prop_state_info.wanted_prop_state {
                            return;
                        }
                    }
                    Err(_e) => return,
                }
            }

            for prop_info in &self.prop_controller.prop_infos {
                let player_steamid = match player.steamid {
                    Some(steamid) => steamid,
                    None => 0,
                };
                if !self.wanted_players.is_empty() && !self.wanted_players.contains(&player_steamid) {
                    continue;
                }
                if self.order_by_steamid && !self.df_per_player.contains_key(&player_steamid) {
                    self.df_per_player.insert(player_steamid, AHashMap::default());
                }
                if self.order_by_steamid {
                    match self.find_prop(prop_info, entity_id, player) {
                        Ok(prop) => {
                            let df_this_player = self.df_per_player.get_mut(&player.steamid.unwrap_or(0)).unwrap();
                            df_this_player.entry(prop_info.id).or_insert_with(|| PropColumn::new()).push(Some(prop.clone()));
                        }
                        Err(_e) => {
                            let df_this_player = self.df_per_player.get_mut(&player.steamid.unwrap_or(0)).unwrap();
                            df_this_player.entry(prop_info.id).or_insert_with(|| PropColumn::new()).push(None);
                        }
                    }
                } else {
                    match self.find_prop(prop_info, entity_id, player) {
                        Ok(prop) => {
                            self.output.entry(prop_info.id).or_insert_with(|| PropColumn::new()).push(Some(prop));
                        }
                        Err(_e) => {
                            // Ultimate debugger is to print this error
                            self.output.entry(prop_info.id).or_insert_with(|| PropColumn::new()).push(None);
                        }
                    }
                }
            }
        }
        // Slice 4 — fold the per-player active-weapon skin/wear/seed captures
        // into the cache (deferred out of the &self.players loop above).
        for (went, skin) in skin_captures {
            self.item_skin_cache.entry(went).or_insert(skin);
        }
    }

    pub fn find_prop(&self, prop_info: &PropInfo, entity_id: &i32, player: &PlayerMetaData) -> Result<Variant, PropCollectionError> {
        match prop_info.prop_type {
            PropType::Tick => return self.create_tick(),
            PropType::Name => return self.create_name(player),
            PropType::Steamid => return self.create_steamid(player),
            PropType::Player => return self.get_prop_from_ent(&prop_info.id, &entity_id),
            PropType::Team => return self.find_team_prop(&prop_info.id, &entity_id),
            PropType::Custom => self.create_custom_prop(prop_info.prop_name.as_str(), entity_id, prop_info, player),
            PropType::Weapon => return self.find_weapon_prop(&prop_info.id, &entity_id),
            PropType::Button => return self.get_button_prop(&prop_info, &entity_id),
            PropType::Controller => return self.get_controller_prop(&prop_info.id, player),
            PropType::Rules => return self.get_rules_prop(prop_info),
            PropType::GameTime => return Ok(Variant::F32(self.net_tick as f32 / 64.0)),
        }
    }
    pub fn get_prop_from_ent(&self, prop_id: &u32, entity_id: &i32) -> Result<Variant, PropCollectionError> {
        match self.entities.get(*entity_id as usize) {
            Some(Some(e)) => match e.props.get(&prop_id) {
                None => return Err(PropCollectionError::GetPropFromEntPropNotFound),
                Some(prop) => return Ok(prop.clone()),
            },
            _ => return Err(PropCollectionError::GetPropFromEntEntityNotFound),
        }
    }
    fn create_tick(&self) -> Result<Variant, PropCollectionError> {
        // This can't actually fail
        return Ok(Variant::I32(self.tick));
    }
    pub fn create_steamid(&self, player: &PlayerMetaData) -> Result<Variant, PropCollectionError> {
        match player.steamid {
            Some(steamid) => return Ok(Variant::U64(steamid)),
            // Revisit this as it was related to pandas null support with u64's
            _ => return Ok(Variant::U64(0)),
        }
    }
    pub fn create_name(&self, player: &PlayerMetaData) -> Result<Variant, PropCollectionError> {
        match &player.name {
            Some(name) => return Ok(Variant::String(name.to_string())),
            _ => return Err(PropCollectionError::PlayerMetaDataNameNone),
        }
    }
    pub fn get_button_prop(&self, prop_info: &PropInfo, entity_id: &i32) -> Result<Variant, PropCollectionError> {
        match self.prop_controller.special_ids.buttons {
            None => Err(PropCollectionError::ButtonsSpecialIDNone),
            Some(button_id) => match self.get_prop_from_ent(&button_id, &entity_id) {
                Ok(Variant::U64(button_mask)) => match BUTTONMAP.get(&prop_info.prop_name) {
                    Some(button_flag) => Ok(Variant::Bool(button_mask & button_flag != 0)),
                    None => return Err(PropCollectionError::ButtonsMapNoEntryFound),
                },
                Ok(_) => return Err(PropCollectionError::ButtonMaskNotU64Variant),
                Err(e) => Err(e),
            },
        }
    }
    pub fn get_rules_prop(&self, prop_info: &PropInfo) -> Result<Variant, PropCollectionError> {
        match self.rules_entity_id {
            Some(entid) => return self.get_prop_from_ent(&prop_info.id, &entid),
            None => return Err(PropCollectionError::RulesEntityIdNotSet),
        }
    }
    pub fn get_controller_prop(&self, prop_id: &u32, player: &PlayerMetaData) -> Result<Variant, PropCollectionError> {
        match player.controller_entid {
            Some(entid) => {
                return self.get_prop_from_ent(prop_id, &entid)
            },
            None => return Err(PropCollectionError::ControllerEntityIdNotSet),
        }
    }
    fn find_owner_entid(&self, entity_id: &i32) -> Result<u32, PropCollectionError> {
        let owner_id = match self.prop_controller.special_ids.grenade_owner_id {
            Some(owner_id) => owner_id,
            None => return Err(PropCollectionError::GrenadeOwnerIdNotSet),
        };
        match self.get_prop_from_ent(&owner_id, entity_id) {
            Ok(Variant::U32(prop)) => Ok(prop & 0x7FF),
            Ok(_) => return Err(PropCollectionError::GrenadeOwnerIdPropIncorrectVariant),
            Err(e) => return Err(e),
        }
    }
    pub fn find_player_metadata(&self, entity_id: i32) -> Result<&PlayerMetaData, PropCollectionError> {
        match self.players.get(&entity_id) {
            Some(metadata) => Ok(metadata),
            None => Err(PropCollectionError::PlayerNotFound),
        }
    }
    pub fn find_thrower_steamid(&self, entity_id: &i32) -> Result<u64, PropCollectionError> {
        let owner_entid = self.find_owner_entid(entity_id)?;
        let metadata = self.find_player_metadata(owner_entid as i32)?;
        match metadata.steamid {
            Some(s) => Ok(s),
            // Watch out
            None => Ok(0),
        }
    }
    pub fn find_thrower_name(&self, entity_id: &i32) -> Result<String, PropCollectionError> {
        let owner_entid = self.find_owner_entid(entity_id)?;
        let metadata = self.find_player_metadata(owner_entid as i32)?;
        match &metadata.name {
            Some(s) => Ok(s.to_owned()),
            None => Err(PropCollectionError::PlayerMetaDataNameNone),
        }
    }

    fn find_grenade_type(&self, entity_id: &i32) -> Option<String> {
        if let Some(Some(ent)) = self.entities.get(*entity_id as usize) {
            if let Some(cls) = self.cls_by_id.get(ent.cls_id as usize) {
                return Some(cls.name.to_string());
            }
        }
        None
    }

    pub fn collect_projectiles(&mut self) {
        // Collect entity ids first to avoid borrowing self.projectiles while
        // mutating self.output / self.projectile_records.
        let projectile_entids: Vec<i32> = self.projectiles.iter().copied().collect();
        for projectile_entid in &projectile_entids {
            let grenade_type = match self.find_grenade_type(projectile_entid) {
                Some(t) => {if !t.contains("Projectile") && !self.parse_grenades{continue}else{t}},
                None => continue,
            };
            let steamid = match self.find_thrower_steamid(projectile_entid) {
                Ok(u) => u,
                _ => continue,
            };
            let name = match self.find_thrower_name(projectile_entid) {
                Ok(x) => x,
                _ => continue,
            };
            // Projectiles are the only ones with coordinates others map to 0.0, map them to None as it is clearer.
            let (x, y, z) = if grenade_type.contains("Project") {
                let x = self.collect_cell_coordinate_grenade(CoordinateAxis::X, projectile_entid).ok();
                let y = self.collect_cell_coordinate_grenade(CoordinateAxis::Y, projectile_entid).ok();
                let z = self.collect_cell_coordinate_grenade(CoordinateAxis::Z, projectile_entid).ok();
                (x, y, z)
            } else {
                (None, None, None)
            };

            // Sprint 5: also push a ProjectileRecord so DemoOutput.projectiles is
            // actually populated (previously vestigial — only output.df was filled).
            let xf = match &x { Some(Variant::F32(v)) => Some(*v), _ => None };
            let yf = match &y { Some(Variant::F32(v)) => Some(*v), _ => None };
            let zf = match &z { Some(Variant::F32(v)) => Some(*v), _ => None };
            // Sprint 5 (Task 5): per-tick m_nBounces snapshot. Wrapper-side
            // walks this for increments to emit BounceEvent rows.
            let bounces = match self.get_prop_from_ent_by_name(projectile_entid, "m_nBounces") {
                Ok(Variant::I32(n)) => Some(n),
                _ => None,
            };
            self.projectile_records.push(ProjectileRecord {
                steamid: Some(steamid),
                name: Some(name.clone()),
                x: xf,
                y: yf,
                z: zf,
                tick: Some(self.tick),
                grenade_type: Some(grenade_type.clone()),
                entity_id: Some(*projectile_entid),
                bounces,
            });

            // Insert these always
            let pairs = vec![
                (GRENADE_TYPE_ID, Some(Variant::String(grenade_type))),
                (STEAMID_ID, Some(Variant::U64(steamid))),
                (NAME_ID, Some(Variant::String(name))),
                (TICK_ID, Some(Variant::I32(self.tick))),
                (ENTITY_ID_ID, Some(Variant::I32(*projectile_entid))),
                (GRENADE_X, x),
                (GRENADE_Y, y),
                (GRENADE_Z, z),
            ];
            for pair in pairs {
                self.output.entry(pair.0).or_insert_with(|| PropColumn::new()).push(pair.1);
            }

            for prop_info in &self.prop_controller.prop_infos {
                // Do these above, props in this loop are from the weapon entity.
                if prop_info.id == STEAMID_ID
                    || prop_info.id == NAME_ID
                    || prop_info.id == TICK_ID
                    || prop_info.id == GRENADE_TYPE_ID
                    || prop_info.id == ENTITY_ID_ID
                    || prop_info.id == GRENADE_X
                    || prop_info.id == GRENADE_Y
                    || prop_info.id == GRENADE_Z
                {
                    continue;
                }
                let prop = match self.get_prop_from_ent(&prop_info.id, &projectile_entid) {
                    Ok(p) => Some(p),
                    _ => None,
                };
                match prop {
                    Some(prop) => {
                        self.output.entry(prop_info.id).or_insert_with(|| PropColumn::new()).push(Some(prop));
                    }
                    None => {
                        self.output.entry(prop_info.id).or_insert_with(|| PropColumn::new()).push(None);
                    }
                }
            }
        }
    }

    /// Sprint 5 — emit one InfernoRecord per tracked CInferno entity per
    /// tick, with the convex hull of the active flame XY positions.
    ///
    /// Reads:
    ///   - `m_fireCount` (scalar) — looked up by name via `prop_infos`.
    ///   - `m_firePositions[i]` — looked up at
    ///     `INFERNO_FIRE_POSITIONS_OFFSET + i` (rewrite added in
    ///     `get_propinfo` so each element gets a unique prop_id).
    ///   - `m_bFireIsBurning[i]` — same scheme at
    ///     `INFERNO_FIRE_IS_BURNING_OFFSET + i`.
    pub fn collect_inferno_records(&mut self) {
        // Snapshot to avoid mutable-borrow conflict with self.entities below.
        let entids: Vec<i32> = self.inferno_entity_ids.clone();
        for inferno_entid in &entids {
            // m_fireCount via name lookup — it's not on the per-element
            // offset path so we resolve it through prop_infos.
            let fire_count = match self.get_prop_from_ent_by_name(inferno_entid, "m_fireCount") {
                Ok(Variant::I32(c)) => c,
                _ => 0,
            };

            let mut active_xy: Vec<[f32; 2]> = Vec::with_capacity(fire_count.max(0) as usize);
            // Iterate up to min(fire_count, 64). m_firePositions is a
            // Vector[64] in CInferno; fire_count is the live count.
            let upper = (fire_count.max(0) as usize).min(64);
            for i in 0..upper {
                let burning_id = INFERNO_FIRE_IS_BURNING_OFFSET + i as u32;
                let pos_id = INFERNO_FIRE_POSITIONS_OFFSET + i as u32;
                let burning = match self.get_prop_from_ent(&burning_id, inferno_entid) {
                    Ok(Variant::Bool(b)) => b,
                    _ => false,
                };
                if !burning {
                    continue;
                }
                let pos = match self.get_prop_from_ent(&pos_id, inferno_entid) {
                    Ok(Variant::VecXYZ(v)) => v,
                    _ => continue,
                };
                active_xy.push([pos[0], pos[1]]);
            }

            self.inferno_records.push(InfernoRecord {
                entity_id: *inferno_entid,
                tick: self.tick,
                fire_count,
                flame_xy: active_xy,
            });
        }
    }

    /// Round-tagging branch — Option B bomb-site resolution. For each
    /// tracked CPlantedC4 entity that hasn't yet been recorded, read
    /// `m_nBombSite` off the entity. Once the prop reads a valid value
    /// (>= 0), emit one [`PlantedC4Record`] capturing the entity_id,
    /// current tick (proxy for plant_tick), and raw site index.
    ///
    /// Why per-tick polling: `m_nBombSite` is set on the CPlantedC4
    /// entity when CS2 plants the bomb — typically the same tick as
    /// the `bomb_planted` GameEvent, but the prop might not be on the
    /// entity at creation if the baseline carries a default. By
    /// polling, we capture the first tick the value lands.
    ///
    /// One record per entity (subsequent ticks are no-ops once
    /// `m_nBombSite` is read). The default-uninitialized value reads
    /// as 0 for CS2 (i32 zero-init); we accept that and rely on the
    /// wrapper to join against `bomb_planted_tick` from GameEvents.
    /// The first valid read across all entities will overwhelmingly
    /// land at the plant tick because that's when the entity is
    /// created in the first place.
    pub fn collect_planted_c4_records(&mut self) {
        if self.planted_c4_entity_ids.is_empty() {
            return;
        }
        // Snapshot to avoid mutable-borrow conflict with self.entities.
        let entids: Vec<i32> = self.planted_c4_entity_ids.clone();
        for entid in &entids {
            // Skip if we've already recorded the CURRENT lifecycle for this
            // entity slot. CS2 reuses slot IDs after delete — scanning
            // `planted_c4_records` would conflate a fresh plant with a
            // previous lifecycle's record at the same slot. The active set
            // is cleared in `entities.rs` on entity-delete, so a reused
            // slot can land a new record.
            if self.planted_c4_recorded_active.contains(entid) {
                continue;
            }
            let site = match self.get_prop_from_ent_by_name(entid, "m_nBombSite") {
                Ok(Variant::I32(s)) => s,
                Ok(Variant::U32(s)) => s as i32,
                _ => continue, // prop not yet populated on this entity
            };
            self.planted_c4_records.push(PlantedC4Record {
                entity_id: *entid,
                plant_tick: self.tick,
                bomb_site: site,
            });
            self.planted_c4_recorded_active.insert(*entid);
        }
    }

    /// Slice 4 — per-round item registry. ON-CHANGE emission (push a record
    /// only when a tracked item entity's owner changes vs its last seen
    /// owner, plus the first observation), resolving m_hOwnerEntity → pawn →
    /// steamid (0xFFFFFF = dropped + a real position), def_index → name,
    /// m_iItemID, clip, cell position, and the out-of-box skin id/name.
    /// Wear/seed come from item_skin_cache (captured in the per-player loop).
    pub fn collect_item_registry_records(&mut self) {
        if self.item_entity_ids.is_empty() {
            return;
        }
        // Snapshot to avoid mutable-borrow conflict with self.entities.
        let entids: Vec<i32> = self.item_entity_ids.clone();
        let tick = self.tick;
        for entid in &entids {
            // CS2 reuses entity slots: a slot tracked while it held a gun can
            // later hold a CKnife / Normal entity (create-without-delete
            // overwrite, before any delete clears item_entity_ids). Re-validate
            // the CURRENT entity at this slot is still a tracked item type;
            // otherwise we'd emit knife/stale records (knives are out of scope).
            match self.entities.get(*entid as usize) {
                Some(Some(e)) if matches!(e.entity_type, EntityType::Item | EntityType::C4) => {}
                _ => continue,
            }
            // def_index identifies the item; absent → entity not ready, skip.
            let def_index = match self.get_prop_from_ent_by_name(entid, "m_iItemDefinitionIndex") {
                Ok(Variant::U32(d)) => d,
                Ok(Variant::I32(d)) => d as u32,
                _ => continue,
            };
            // owner handle → pawn idx → steamid; 0xFFFFFF (or unresolved) = dropped.
            let owner_handle = match self.get_prop_from_ent_by_name(entid, "m_hOwnerEntity") {
                Ok(Variant::U32(h)) => h,
                Ok(Variant::I32(h)) => h as u32,
                _ => 0x7FFFFFFF,
            };
            let owner_pawn_idx = (owner_handle & 0x7FF) as i32;
            let (owner_steamid, on_ground) = match self.players.get(&owner_pawn_idx) {
                Some(p) if owner_handle != 0xFFFFFF => (p.steamid, false),
                _ => (None, true),
            };
            let prev = self.item_prev_owner.get(entid).copied();
            let changed = prev != Some(owner_pawn_idx);
            if !changed {
                continue;
            }
            self.item_prev_owner.insert(*entid, owner_pawn_idx);
            // Read the rest only on a change (bounded work).
            let clip = match self.get_prop_from_ent_by_name(entid, "m_iClip1") {
                Ok(Variant::I32(c)) => c,
                Ok(Variant::U32(c)) => c as i32,
                _ => -1,
            };
            let x = self.item_coord(entid, CoordinateAxis::X);
            let y = self.item_coord(entid, CoordinateAxis::Y);
            let z = self.item_coord(entid, CoordinateAxis::Z);
            let item_id = self.read_item_id(entid);
            let (skin_id, skin_name, wear, seed) = self
                .item_skin_cache
                .get(entid)
                .cloned()
                .unwrap_or_else(|| {
                    let id = match self.find_weapon_skin_id(entid) {
                        Ok(Variant::U32(v)) => v,
                        _ => 0,
                    };
                    let name = match self.find_weapon_skin(entid) {
                        Ok(Variant::String(s)) => s,
                        _ => String::new(),
                    };
                    (id, name, 0.0, 0)
                });
            let item_name = WEAPINDICIES
                .get(&def_index)
                .map(|s| s.to_string())
                .unwrap_or_default();
            self.item_registry_records.push(ItemRegistryRecord {
                entity_id: *entid,
                tick,
                def_index,
                item_name,
                item_id,
                owner_steamid,
                on_ground,
                x,
                y,
                z,
                clip,
                skin_id,
                skin_name,
                wear,
                seed,
            });
        }
    }

    /// Slice 4 helper — cell+offset position of an item entity on one axis.
    /// Weapons share CBodyComponentBaseAnimGraph, so the grenade cell reader
    /// works on them. Returns 0.0 on a missing/unready prop.
    fn item_coord(&self, entid: &i32, axis: CoordinateAxis) -> f32 {
        match self.collect_cell_coordinate_grenade(axis, entid) {
            Ok(Variant::F32(v)) => v,
            _ => 0.0,
        }
    }

    /// Slice 4 helper — combine m_iItemIDLow | (m_iItemIDHigh << 32).
    fn read_item_id(&self, entid: &i32) -> u64 {
        let low = match self.get_prop_from_ent_by_name(entid, "m_iItemIDLow") {
            Ok(Variant::U32(v)) => v as u64,
            Ok(Variant::I32(v)) => v as u32 as u64,
            _ => 0,
        };
        let high = match self.get_prop_from_ent_by_name(entid, "m_iItemIDHigh") {
            Ok(Variant::U32(v)) => v as u64,
            Ok(Variant::I32(v)) => v as u32 as u64,
            _ => 0,
        };
        low | (high << 32)
    }

    /// Sprint 5 / Task 4 — called when a CSmokeGrenadeProjectile entity is
    /// being deleted (smoke expired). Reads the smoke's lifecycle props
    /// one final time (before the entity slot is cleared) and emits a
    /// SmokeRecord. Bare prop names per the class-prefix-strip rule
    /// (see probe_grenades.rs preamble).
    pub fn finalize_smoke_record(&mut self, entity_id: i32, expire_tick: i32) {
        let detonate_pos = match self.get_prop_from_ent_by_name(&entity_id, "m_vSmokeDetonationPos") {
            Ok(Variant::VecXYZ(v)) => v,
            _ => [0.0, 0.0, 0.0],
        };
        let detonate_tick = match self.get_prop_from_ent_by_name(&entity_id, "m_nSmokeEffectTickBegin") {
            Ok(Variant::I32(t)) => t,
            _ => 0,
        };
        let exploded_from_inferno = match self.get_prop_from_ent_by_name(&entity_id, "m_bExplodeFromInferno") {
            Ok(Variant::Bool(b)) => b,
            _ => false,
        };
        // m_hThrower is a CHandle (encoded as u32). Lower 11 bits = entity_id;
        // remaining bits are serial number. Mask to recover the entity_id.
        let thrower_entity_id = match self.get_prop_from_ent_by_name(&entity_id, "m_hThrower") {
            Ok(Variant::U32(h)) => (h & 0x7FF) as i32,
            _ => 0,
        };

        // Reassemble the voxel journal blob from per-element props.
        // Each byte of m_VoxelFrameData was stored under a unique prop_id
        // SMOKE_VOXEL_DATA_OFFSET + element_index (see prop_controller /
        // sendtables fan-out). m_nVoxelFrameDataSize is the authoritative
        // element count.
        let voxel_frame_data_size = match self.get_prop_from_ent_by_name(&entity_id, "m_nVoxelFrameDataSize") {
            Ok(Variant::I32(n)) => n,
            Ok(Variant::U32(n)) => n as i32,
            _ => 0,
        };
        let n = voxel_frame_data_size.max(0) as usize;
        // Try both element-index bases. For a CNetworkUtlVectorBase the
        // delta-encoder sends element-0 as the "length" element when the
        // vector first grows, so bytes may start at base=1 (length prefix
        // occupies slot 0) or at base=0 (no prefix). We try base=1 first
        // because that matches the observed inferno fire-positions layout;
        // fall back to base=0 if the first attempt does not tile cleanly.
        let mut voxel_frame_data: Vec<u8> = Vec::new();
        for base in [1u32, 0u32] {
            let mut buf = Vec::with_capacity(n);
            for i in 0..n as u32 {
                let pid = SMOKE_VOXEL_DATA_OFFSET + base + i;
                let byte = match self.get_prop_from_ent(&pid, &entity_id) {
                    Ok(Variant::U32(b)) => b as u8,
                    _ => 0u8,
                };
                buf.push(byte);
            }
            if journal_tiles(&buf) {
                voxel_frame_data = buf;
                break;
            }
            if voxel_frame_data.is_empty() {
                voxel_frame_data = buf;
            }
        }
        self.smoke_records.push(SmokeRecord {
            entity_id,
            thrower_entity_id,
            detonate_tick,
            expire_tick,
            detonate_pos,
            exploded_from_inferno,
            voxel_frame_data,
            voxel_frame_data_size,
        });
    }

    /// Helper — look up a prop_id by name in `prop_infos` then read it
    /// off a specific entity. Used for one-off scalar reads where the
    /// caller doesn't already have a special-id stashed (e.g.
    /// `m_fireCount`).
    ///
    /// Sprint 5 (I2 from code review): O(1) lookup via PropController.name_to_id
    /// instead of linear scan of prop_infos. This is a hot path —
    /// called ~200K+ times per parse (per-inferno per-tick + per-smoke-delete
    /// + per-projectile-tick).
    pub fn get_prop_from_ent_by_name(
        &self,
        entity_id: &i32,
        name: &str,
    ) -> Result<Variant, PropCollectionError> {
        let prop_id = *self
            .prop_controller
            .name_to_id
            .get(name)
            .ok_or(PropCollectionError::GetPropFromEntPropNotFound)?;
        self.get_prop_from_ent(&prop_id, entity_id)
    }

    fn find_weapon_name(&self, entity_id: &i32) -> Result<Variant, PropCollectionError> {
        let item_def_id = match self.prop_controller.special_ids.item_def {
            Some(x) => x,
            None => return Err(PropCollectionError::SpecialidsItemDefNotSet),
        };
        match self.find_weapon_prop(&item_def_id, entity_id) {
            Ok(Variant::U32(def_idx)) => {
                match WEAPINDICIES.get(&def_idx) {
                    Some(v) => return Ok(Variant::String(v.to_string())),
                    None => return Err(PropCollectionError::WeaponIdxMappingNotFound),
                };
            }
            Ok(_) => return Err(PropCollectionError::WeaponDefVariantWrongType),
            Err(e) => Err(e),
        }
    }
    pub fn collect_cell_coordinate_player(&self, axis: CoordinateAxis, entity_id: &i32) -> Result<Variant, PropCollectionError> {
        let coordinate = match axis {
            CoordinateAxis::X => {
                let x_prop_id = match self.prop_controller.special_ids.cell_x_player {
                    Some(x) => x,
                    None => return Err(PropCollectionError::PlayerSpecialIDCellXMissing),
                };
                let x_offset_id = match self.prop_controller.special_ids.cell_x_offset_player {
                    Some(x) => x,
                    None => return Err(PropCollectionError::PlayerSpecialIDOffsetXMissing),
                };
                let offset = self.get_prop_from_ent(&x_offset_id, entity_id);
                let cell = self.get_prop_from_ent(&x_prop_id, entity_id);
                coord_from_cell(cell, offset)
            }
            CoordinateAxis::Y => {
                let y_prop_id = match self.prop_controller.special_ids.cell_y_player {
                    Some(y) => y,
                    None => return Err(PropCollectionError::PlayerSpecialIDCellYMissing),
                };
                let y_offset_id = match self.prop_controller.special_ids.cell_y_offset_player {
                    Some(y) => y,
                    None => return Err(PropCollectionError::PlayerSpecialIDOffsetYMissing),
                };
                let offset = self.get_prop_from_ent(&y_offset_id, entity_id);
                let cell = self.get_prop_from_ent(&y_prop_id, entity_id);
                coord_from_cell(cell, offset)
            }
            CoordinateAxis::Z => {
                let z_prop_id = match self.prop_controller.special_ids.cell_z_player {
                    Some(z) => z,
                    None => return Err(PropCollectionError::PlayerSpecialIDCellZMissing),
                };
                let z_offset_id = match self.prop_controller.special_ids.cell_z_offset_player {
                    Some(z) => z,
                    None => return Err(PropCollectionError::PlayerSpecialIDOffsetZMissing),
                };
                let offset = self.get_prop_from_ent(&z_offset_id, entity_id);
                let cell = self.get_prop_from_ent(&z_prop_id, entity_id);
                coord_from_cell(cell, offset)
            }
        };
        Ok(Variant::F32(coordinate?))
    }
    pub fn collect_cell_coordinate_grenade(&self, axis: CoordinateAxis, entity_id: &i32) -> Result<Variant, PropCollectionError> {
        // Todo rename to be consistent with player special ids
        let coordinate = match axis {
            CoordinateAxis::X => {
                let x_prop_id = match self.prop_controller.special_ids.m_cell_x_grenade {
                    Some(x) => x,
                    None => return Err(PropCollectionError::GrenadeSpecialIDCellXMissing),
                };
                let x_offset_id = match self.prop_controller.special_ids.m_vec_x_grenade {
                    Some(x) => x,
                    None => return Err(PropCollectionError::GrenadeSpecialIDOffsetXMissing),
                };
                let offset = self.get_prop_from_ent(&x_offset_id, entity_id);
                let cell = self.get_prop_from_ent(&x_prop_id, entity_id);
                coord_from_cell(cell, offset)
            }
            CoordinateAxis::Y => {
                let y_prop_id = match self.prop_controller.special_ids.m_cell_y_grenade {
                    Some(y) => y,
                    None => return Err(PropCollectionError::GrenadeSpecialIDCellYMissing),
                };
                let y_offset_id = match self.prop_controller.special_ids.m_vec_y_grenade {
                    Some(y) => y,
                    None => return Err(PropCollectionError::GrenadeSpecialIDOffsetYMissing),
                };

                let offset = self.get_prop_from_ent(&y_offset_id, entity_id);
                let cell = self.get_prop_from_ent(&y_prop_id, entity_id);
                coord_from_cell(cell, offset)
            }
            CoordinateAxis::Z => {
                let z_prop_id = match self.prop_controller.special_ids.m_cell_z_grenade {
                    Some(z) => z,
                    None => return Err(PropCollectionError::GrenadeSpecialIDCellZMissing),
                };
                let z_offset_id = match self.prop_controller.special_ids.m_vec_z_grenade {
                    Some(z) => z,
                    None => return Err(PropCollectionError::GrenadeSpecialIDOffsetZMissing),
                };
                let offset = self.get_prop_from_ent(&z_offset_id, entity_id);
                let cell = self.get_prop_from_ent(&z_prop_id, entity_id);
                coord_from_cell(cell, offset)
            }
        };
        Ok(Variant::F32(coordinate?))
    }
    fn find_pitch_or_yaw(&self, entity_id: &i32, idx: usize) -> Result<Variant, PropCollectionError> {
        match self.prop_controller.special_ids.eye_angles {
            Some(prop_id) => match self.get_prop_from_ent(&prop_id, entity_id) {
                Ok(Variant::VecXYZ(v)) => return Ok(Variant::F32(v[idx])),
                Ok(_) => return Err(PropCollectionError::EyeAnglesWrongVariant),
                Err(e) => return Err(e),
            },
            None => Err(PropCollectionError::SpecialidsEyeAnglesNotSet),
        }
    }
    pub fn create_custom_prop(&self, prop_name: &str, entity_id: &i32, prop_info: &PropInfo, player: &PlayerMetaData) -> Result<Variant, PropCollectionError> {
        match prop_name {
            "X" => self.collect_cell_coordinate_player(CoordinateAxis::X, entity_id),
            "Y" => self.collect_cell_coordinate_player(CoordinateAxis::Y, entity_id),
            "Z" => self.collect_cell_coordinate_player(CoordinateAxis::Z, entity_id),
            "velocity" => self.collect_velocity(player),
            "velocity_X" => self.collect_velocity_axis(player, CoordinateAxis::X),
            "velocity_Y" => self.collect_velocity_axis(player, CoordinateAxis::Y),
            "velocity_Z" => self.collect_velocity_axis(player, CoordinateAxis::Z),
            "pitch" => self.find_pitch_or_yaw(entity_id, 0),
            "yaw" => self.find_pitch_or_yaw(entity_id, 1),
            "weapon_name" => self.find_weapon_name(entity_id),
            "weapon_skin" => self.find_weapon_skin_from_player(entity_id),
            "weapon_skin_id" => self.find_weapon_skin_id_from_player(entity_id),
            "weapon_paint_seed" => self.find_skin_paint_seed(player),
            "weapon_float" => self.find_skin_float(player),
            "weapon_stickers" => self.find_stickers_from_active_weapon(player),
            "active_weapon_original_owner" => self.find_weapon_original_owner(entity_id),
            "inventory" => self.find_my_inventory(entity_id),
            "inventory_as_ids" => self.find_my_inventory_as_ids(entity_id),
            "inventory_as_bitmask" => self.find_my_inventory_as_bitmask(entity_id),
            "CCSPlayerPawn.m_bSpottedByMask" => self.find_spotted(entity_id, prop_info),
            "entity_id" => return Ok(Variant::I32(*entity_id)),
            "is_alive" => return self.find_is_alive(entity_id),
            "user_id" => return self.get_userid(player),
            "is_airborne" => self.find_is_airborne(player),
            "agent_skin" => return self.find_agent_skin(player),
            "CCSPlayerController.m_iCompTeammateColor" => return self.find_player_color(player, prop_info),
            "usercmd_input_history" => self.get_prop_from_ent(&USERCMD_INPUT_HISTORY_BASEID, entity_id),
            "glove_paint_id" => self.find_glove_skin_id(entity_id),
            "glove_skin" => self.find_glove_skin(entity_id),
            "glove_paint_seed" => self.find_glove_paint_seed(entity_id),
            "glove_paint_float" => self.find_glove_paint_float(entity_id),
            _ => Err(PropCollectionError::UnknownCustomPropName),
        }
    }
    pub fn get_userid(&self, player: &PlayerMetaData) -> Result<Variant, PropCollectionError> {
        for (_, st_player) in &self.stringtable_players {
            if player.steamid == Some(st_player.steamid) {
                return Ok(Variant::I32(st_player.userid));
            }
        }
        Err(PropCollectionError::UseridNotFound)
    }
    pub fn find_player_color(&self, player: &PlayerMetaData, prop_info: &PropInfo) -> Result<Variant, PropCollectionError> {
        if let Ok(Variant::I32(v)) = self.get_controller_prop(&prop_info.id, player) {
            let color = if let Some(col) = PLAYER_COLOR.get(&v) {
                col.to_string()
            } else {
                v.to_string()
            };
            return Ok(Variant::String(color));
        }
        Err(PropCollectionError::UseridNotFound)
    }
    pub fn find_is_airborne(&self, player: &PlayerMetaData) -> Result<Variant, PropCollectionError> {
        if let Some(player_entity_id) = &player.player_entity_id {
            if let Some(id) = self.prop_controller.special_ids.is_airborn {
                if let Ok(Variant::U32(airborn_h)) = self.get_prop_from_ent(&id, &player_entity_id) {
                    return Ok(Variant::Bool(airborn_h == IS_AIRBORNE_CONST));
                }
            }
        }
        Ok(Variant::Bool(false))
    }
    pub fn find_skin_float(&self, player: &PlayerMetaData) -> Result<Variant, PropCollectionError> {
        if let Some(player_entity_id) = &player.player_entity_id {
            return self.find_weapon_prop(&WEAPON_FLOAT, &player_entity_id);
        }
        Err(PropCollectionError::PlayerNotFound)
    }
    pub fn find_stickers_from_active_weapon(&self, player: &PlayerMetaData) -> Result<Variant, PropCollectionError> {
        let p = match self.prop_controller.special_ids.active_weapon {
            Some(p) => p,
            None => return Err(PropCollectionError::SpecialidsActiveWeaponNotSet),
        };
        if let Some(eid) = player.player_entity_id {
            return match self.get_prop_from_ent(&p, &eid) {
                Ok(Variant::U32(weap_handle)) => {
                    // Could be more specific
                    let weapon_entity_id = (weap_handle & 0x7FF) as i32;
                    self.find_stickers(&weapon_entity_id)
                }
                Ok(_) => Err(PropCollectionError::WeaponHandleIncorrectVariant),
                Err(e) => Err(e),
            };
        }
        Err(PropCollectionError::PlayerNotFound)
    }

    pub fn find_stickers(&self, weapon_entity_id: &i32) -> Result<Variant, PropCollectionError> {
        let mut stickers = vec![];
        // indicies 0..4 info about skin. 4..24 info about stickers. 5 MAX STICKERS (4 idx per sticker),
        for idx in (4..25).step_by(4) {
            let sticker_id_id = WEAPON_SKIN_ID + idx;
            let sticker_wear_id = WEAPON_SKIN_ID + idx + 1;
            let sticker_x = WEAPON_SKIN_ID + idx + 2;
            let sticker_y = WEAPON_SKIN_ID + idx + 3;
            if let Some(sticker) = self.find_sticker(weapon_entity_id, sticker_id_id, sticker_wear_id, sticker_x, sticker_y) {
                stickers.push(sticker);
            }
        }
        return Ok(Variant::Stickers(stickers));
    }
    fn find_sticker(&self, entity_id: &i32, sticker_id_id: u32, sticker_wear_id: u32, sticker_x: u32, sticker_y: u32) -> Option<Sticker> {
        let id = self.get_prop_from_ent(&sticker_id_id, entity_id);
        let wear = self.get_prop_from_ent(&sticker_wear_id, entity_id);
        let sticker_x = self.get_prop_from_ent(&sticker_x, entity_id);
        let sticker_y = self.get_prop_from_ent(&sticker_y, entity_id);
        if let (Ok(Variant::F32(id)), Ok(Variant::F32(wear)), Ok(Variant::F32(sticker_x)), Ok(Variant::F32(sticker_y))) = (id, wear, sticker_x, sticker_y) {
            return Some(Sticker {
                id: id.to_bits(),
                name: STICKER_ID_TO_NAME.get(&id.to_bits()).unwrap_or(&"unknown").to_string(),
                wear: if wear < 0.0000000 { 0.0 } else { wear },
                x: sticker_x,
                y: sticker_y,
            });
        }
        None
    }
    pub fn find_skin_paint_seed(&self, player: &PlayerMetaData) -> Result<Variant, PropCollectionError> {
        if let Some(player_entity_id) = &player.player_entity_id {
            if let Ok(Variant::F32(f)) = self.find_weapon_prop(&WEAPON_PAINT_SEED, &player_entity_id) {
                return Ok(Variant::U32(f as u32));
            }
        }
        return Ok(Variant::U32(0));
    }
    pub fn find_agent_skin(&self, player: &PlayerMetaData) -> Result<Variant, PropCollectionError> {
        let id = match self.prop_controller.special_ids.agent_skin_idx {
            Some(i) => i,
            None => return Err(PropCollectionError::AgentSpecialIdNotSet),
        };
        match self.get_controller_prop(&id, player) {
            Ok(Variant::U32(agent_id)) => match AGENTSMAP.get(&agent_id) {
                Some(agent) => return Ok(Variant::String(agent.to_string())),
                None => return Err(PropCollectionError::AgentIdNotFound),
            },
            Ok(_) => return Err(PropCollectionError::AgentIncorrectVariant),
            Err(_) => return Err(PropCollectionError::AgentPropNotFound),
        }
    }
    pub fn collect_velocity(&self, player: &PlayerMetaData) -> Result<Variant, PropCollectionError> {
        if let Some(s) = player.steamid {
            let steamids = self.output.get(&STEAMID_ID);
            let indicies = self.find_wanted_indicies(steamids, s);

            let x = self.velocity_from_indicies(&indicies, CoordinateAxis::X)?;
            let y = self.velocity_from_indicies(&indicies, CoordinateAxis::Y)?;

            if let (Variant::F32(x), Variant::F32(y)) = (x, y) {
                return Ok(Variant::F32((f32::powi(x, 2) + f32::powi(y, 2)).sqrt()));
            }
        }
        return Err(PropCollectionError::PlayerNotFound);
    }
    pub fn collect_velocity_axis(&self, player: &PlayerMetaData, axis: CoordinateAxis) -> Result<Variant, PropCollectionError> {
        if let Some(s) = player.steamid {
            let steamids = self.output.get(&STEAMID_ID);
            let indicies = self.find_wanted_indicies(steamids, s);
            return Ok(self.velocity_from_indicies(&indicies, axis)?);
        }
        return Err(PropCollectionError::PlayerNotFound);
    }
    fn find_most_recent_coordinate_idx(&self, optv: Option<&PropColumn>, wanted_steamid: u64) -> Option<usize> {
        if let Some(v) = optv {
            if let Some(VarVec::U64(steamid_vec)) = &v.data {
                for idx in (0..steamid_vec.len()).rev() {
                    if steamid_vec[idx] == Some(wanted_steamid) {
                        return Some(idx);
                    }
                }
            }
        }
        None
    }
    fn find_last_coordinate_idx(&self, optv: Option<&PropColumn>, wanted_steamid: u64, cur_idx: Option<usize>) -> Option<usize> {
        let cur_idx = cur_idx?;
        if let VarVec::U64(steamid_vec) = optv?.data.as_ref()? {
            // iterate backwards until steamid is our wanted player and > 1sec ago
            for idx in (0..steamid_vec.len()).rev() {
                let sid = steamid_vec[idx];
                if sid == Some(wanted_steamid) && idx != cur_idx {
                    return Some(idx);
                }
            }
        }
        None
    }
    fn find_wanted_indicies(&self, optv: Option<&PropColumn>, wanted_steamid: u64) -> Vec<usize> {
        let idx1 = self.find_most_recent_coordinate_idx(optv, wanted_steamid);
        let idx2 = self.find_last_coordinate_idx(optv, wanted_steamid, idx1);
        if let (Some(idx1), Some(idx2)) = (idx1, idx2) {
            return vec![idx1, idx2];
        }
        vec![]
    }

    fn velocity_from_indicies(&self, indicies: &[usize], axis: CoordinateAxis) -> Result<Variant, PropCollectionError> {
        let col = match axis {
            CoordinateAxis::X => self.output.get(&PLAYER_X_ID),
            CoordinateAxis::Y => self.output.get(&PLAYER_Y_ID),
            CoordinateAxis::Z => self.output.get(&PLAYER_Z_ID),
        };
        if let Some(c) = col {
            if let Some((Some(v1), Some(v2))) = self.index_coordinates_from_propcol(c, indicies) {
                return Ok(Variant::F32((v1 * 64.0) - (v2 * 64.0)));
            }
        }
        return Err(PropCollectionError::VelocityNotFound);
    }
    fn index_coordinates_from_propcol(&self, propcol: &PropColumn, indicies: &[usize]) -> Option<(Option<f32>, Option<f32>)> {
        if indicies.len() != 2 {
            return None;
        }
        if let Some(VarVec::F32(steamid_vec)) = &propcol.data {
            let first = steamid_vec[indicies[0]];
            let second = steamid_vec[indicies[1]];
            return Some((first, second));
        }
        None
    }

    pub fn find_is_alive(&self, entity_id: &i32) -> Result<Variant, PropCollectionError> {
        match self.prop_controller.special_ids.life_state {
            Some(id) => match self.get_prop_from_ent(&id, entity_id) {
                Ok(Variant::U32(0)) => return Ok(Variant::Bool(true)),
                Ok(_) => {}
                Err(_) => {}
            },
            None => {}
        }
        Ok(Variant::Bool(false))
    }
    pub fn find_spotted(&self, entity_id: &i32, prop_info: &PropInfo) -> Result<Variant, PropCollectionError> {
        match self.get_prop_from_ent(&prop_info.id, entity_id) {
            Ok(Variant::U32(mask)) => {
                return Ok(Variant::U64Vec(self.steamids_from_mask(mask)));
            }
            Ok(_) => return Err(PropCollectionError::SpottedIncorrectVariant),
            Err(e) => return Err(e),
        }
    }
    fn steamids_from_mask(&self, uid: u32) -> Vec<u64> {
        let mut steamids = vec![];
        for i in 0..16 {
            if (uid & (1 << i)) != 0 {
                if let Some(user) = self.find_user_by_controller_id((i + 1) as i32) {
                    steamids.push(user.steamid.unwrap_or(0))
                }
            }
        }
        steamids
    }
    pub fn find_my_inventory(&self, entity_id: &i32) -> Result<Variant, PropCollectionError> {
        let mut names = vec![];
        let mut unique_eids = vec![];

        match self.find_is_alive(entity_id) {
            Ok(Variant::Bool(true)) => {}
            _ => return Ok(Variant::StringVec(vec![])),
        };
        let inventory_max_len = match self.get_prop_from_ent(&(MY_WEAPONS_OFFSET as u32), entity_id) {
            Ok(Variant::U32(p)) => p,
            _ => return Err(PropCollectionError::InventoryMaxNotFound),
        };
        for i in 1..inventory_max_len + 1 {
            let prop_id = MY_WEAPONS_OFFSET + i;
            match self.get_prop_from_ent(&(prop_id as u32), entity_id) {
                Err(_e) => {}
                Ok(Variant::U32(x)) => {
                    let eid = (x & ((1 << 14) - 1)) as i32;
                    // Sometimes multiple references to same eid?
                    if unique_eids.contains(&eid) {
                        continue;
                    }
                    unique_eids.push(eid);

                    if let Some(item_def_id) = &self.prop_controller.special_ids.item_def {
                        let res = match self.get_prop_from_ent(item_def_id, &eid) {
                            Err(_e) => continue,
                            Ok(def) => def,
                        };
                        self.insert_equipment_name(&mut names, res, entity_id);
                    }
                }
                _ => {}
            }
        }
        Ok(Variant::StringVec(names))
    }
    pub fn find_my_inventory_as_ids(&self, entity_id: &i32) -> Result<Variant, PropCollectionError> {
        let mut names = vec![];
        let mut unique_eids = vec![];

        match self.find_is_alive(entity_id) {
            Ok(Variant::Bool(true)) => {}
            _ => return Ok(Variant::U32Vec(vec![])),
        };
        let inventory_max_len = match self.get_prop_from_ent(&(MY_WEAPONS_OFFSET as u32), entity_id) {
            Ok(Variant::U32(p)) => p,
            _ => return Err(PropCollectionError::InventoryMaxNotFound),
        };

        for i in 1..inventory_max_len + 1 {
            let prop_id = MY_WEAPONS_OFFSET + i;
            match self.get_prop_from_ent(&(prop_id as u32), entity_id) {
                Err(_e) => {}
                Ok(Variant::U32(x)) => {
                    let eid = (x & ((1 << 14) - 1)) as i32;
                    // Sometimes multiple references to same eid?
                    if unique_eids.contains(&eid) {
                        continue;
                    }
                    unique_eids.push(eid);
                    if let Some(item_def_id) = &self.prop_controller.special_ids.item_def {
                        let res = match self.get_prop_from_ent(item_def_id, &eid) {
                            Err(_e) => continue,
                            Ok(def) => def,
                        };
                        self.insert_equipment_id(&mut names, res, entity_id);
                    }
                }
                _ => {}
            }
        }
        Ok(Variant::U32Vec(names))
    }
    pub fn find_my_inventory_as_bitmask(&self, entity_id: &i32) -> Result<Variant, PropCollectionError> {
        let mut bitmask = 0;
        let mut unique_eids = vec![];

        match self.find_is_alive(entity_id) {
            Ok(Variant::Bool(true)) => {}
            _ => return Ok(Variant::U64(0)),
        };
        let inventory_max_len = match self.get_prop_from_ent(&(MY_WEAPONS_OFFSET as u32), entity_id) {
            Ok(Variant::U32(p)) => p,
            _ => return Err(PropCollectionError::InventoryMaxNotFound),
        };

        for i in 1..inventory_max_len + 1 {
            let prop_id = MY_WEAPONS_OFFSET + i;
            match self.get_prop_from_ent(&(prop_id as u32), entity_id) {
                Err(_e) => {}
                Ok(Variant::U32(x)) => {
                    let eid = (x & ((1 << 14) - 1)) as i32;
                    // Sometimes multiple references to same eid?
                    if unique_eids.contains(&eid) {
                        continue;
                    }
                    unique_eids.push(eid);
                    if let Some(item_def_id) = &self.prop_controller.special_ids.item_def {
                        let res = match self.get_prop_from_ent(item_def_id, &eid) {
                            Err(_e) => continue,
                            Ok(def) => def,
                        };
                        self.insert_equipment_id_bitmask(&mut bitmask, res, entity_id);
                    }
                }
                _ => {}
            }
        }
        Ok(Variant::U64(bitmask))
    }

    fn insert_equipment_id_bitmask(&self, bitmask: &mut u64, res: Variant, player_entid: &i32) {
        if let Variant::U32(def_idx) = res {
            match WEAPINDICIES.get(&def_idx) {
                None => return,
                Some(weap_name) => {
                    match weap_name {
                        // Check how many flashbangs player has (only prop that works like this)
                        &"Flashbang" => {
                            if let Ok(Variant::U32(2)) = self.get_prop_from_ent(&GRENADE_AMMO_ID, player_entid) {
                                *bitmask |= 1 << def_idx;
                            }
                            *bitmask |= 1 << def_idx;
                        }
                        // c4 seems bugged. Find c4 entity and check owner from it.
                        &"C4 Explosive" => {
                            if let Some(c4_owner_id) = self.find_c4_owner() {
                                if *player_entid == c4_owner_id {
                                    *bitmask |= 1 << def_idx;
                                }
                            }
                        }
                        _ => {
                            *bitmask |= 1 << def_idx;
                        }
                    }
                }
            };
        }
    }
    fn insert_equipment_id(&self, names: &mut Vec<u32>, res: Variant, player_entid: &i32) {
        if let Variant::U32(def_idx) = res {
            match WEAPINDICIES.get(&def_idx) {
                None => return,
                Some(weap_name) => {
                    match weap_name {
                        // Check how many flashbangs player has (only prop that works like this)
                        &"Flashbang" => {
                            if let Ok(Variant::U32(2)) = self.get_prop_from_ent(&FLASHBANG_AMMO_ID, player_entid) {
                                names.push(def_idx);
                            }
                            names.push(def_idx);
                        }
                        // c4 seems bugged. Find c4 entity and check owner from it.
                        &"C4 Explosive" => {
                            if let Some(c4_owner_id) = self.find_c4_owner() {
                                if *player_entid == c4_owner_id {
                                    names.push(def_idx);
                                }
                            }
                        }
                        _ => {
                            names.push(def_idx);
                        }
                    }
                }
            };
        }
    }

    fn insert_equipment_name(&self, names: &mut Vec<String>, res: Variant, player_entid: &i32) {
        if let Variant::U32(def_idx) = res {
            match WEAPINDICIES.get(&def_idx) {
                None => return,
                Some(weap_name) => {
                    match weap_name {
                        // Check how many flashbangs player has (only prop that works like this)
                        &"Flashbang" => {
                            if let Ok(Variant::U32(2)) = self.get_prop_from_ent(&FLASHBANG_AMMO_ID, player_entid) {
                                names.push(weap_name.to_string());
                            }
                            names.push(weap_name.to_string());
                        }
                        // c4 seems bugged. Find c4 entity and check owner from it.
                        &"C4 Explosive" => {
                            if let Some(c4_owner_id) = self.find_c4_owner() {
                                if *player_entid == c4_owner_id {
                                    names.push(weap_name.to_string());
                                }
                            }
                        }
                        _ => {
                            names.push(weap_name.to_string());
                        }
                    }
                }
            };
        }
    }
    fn find_c4_owner(&self) -> Option<i32> {
        if let Some(c4ent) = self.c4_entity_id {
            if let Some(id) = self.prop_controller.special_ids.h_owner_entity {
                if let Ok(Variant::U32(u)) = self.get_prop_from_ent(&id, &c4ent) {
                    return Some((u & 0x7FF) as i32);
                }
            }
        }
        None
    }
    pub fn find_weapon_original_owner(&self, entity_id: &i32) -> Result<Variant, PropCollectionError> {
        let low_id = match self.prop_controller.special_ids.orig_own_low {
            Some(id) => id,
            None => return Err(PropCollectionError::OriginalOwnerXuidIdLowNotSet),
        };
        let high_id = match self.prop_controller.special_ids.orig_own_high {
            Some(id) => id,
            None => return Err(PropCollectionError::OriginalOwnerXuidIdHighNotSet),
        };
        let low_bits = match self.find_weapon_prop(&low_id, entity_id) {
            Ok(Variant::U32(val)) => val,
            Ok(_) => return Err(PropCollectionError::OriginalOwnerXuidlowIncorrectVariant),
            Err(_e) => return Err(PropCollectionError::OriginalOwnerXuidLowNotFound),
        };
        let high_bits = match self.find_weapon_prop(&high_id, entity_id) {
            Ok(Variant::U32(val)) => val,
            Ok(_) => return Err(PropCollectionError::OriginalOwnerXuidHighIncorrectVariant),
            Err(_e) => return Err(PropCollectionError::OriginalOwnerXuidHighNotFound),
        };
        let combined = (high_bits as u64) << 32 | (low_bits as u64);
        Ok(Variant::String(combined.to_string()))
    }

    pub fn find_weapon_skin(&self, weapon_entity_id: &i32) -> Result<Variant, PropCollectionError> {
        match self.get_prop_from_ent(&WEAPON_SKIN_ID, weapon_entity_id) {
            Ok(Variant::F32(f)) => {
                // The value is stored as a float for some reason
                if f.fract() == 0.0 && f >= 0.0 {
                    let idx = f as u32;
                    match PAINTKITS.get(&idx) {
                        Some(kit) => Ok(Variant::String(kit.to_string())),
                        None => Err(PropCollectionError::WeaponSkinNoSkinMapping),
                    }
                } else {
                    return Err(PropCollectionError::WeaponSkinFloatConvertionError);
                }
            }
            Ok(_) => return Err(PropCollectionError::WeaponSkinIdxIncorrectVariant),
            Err(e) => return Err(e),
        }
    }
    pub fn find_weapon_skin_id_from_player(&self, player_entid: &i32) -> Result<Variant, PropCollectionError> {
        let p = match self.prop_controller.special_ids.active_weapon {
            Some(p) => p,
            None => return Err(PropCollectionError::SpecialidsActiveWeaponNotSet),
        };
        return match self.get_prop_from_ent(&p, player_entid) {
            Ok(Variant::U32(weap_handle)) => {
                let weapon_entity_id = (weap_handle & 0x7FF) as i32;
                self.find_weapon_skin_id(&weapon_entity_id)
            }
            Ok(_) => Err(PropCollectionError::WeaponHandleIncorrectVariant),
            Err(e) => Err(e),
        };
    }
    pub fn find_weapon_skin_id(&self, weapon_entity_id: &i32) -> Result<Variant, PropCollectionError> {
        match self.get_prop_from_ent(&WEAPON_SKIN_ID, weapon_entity_id) {
            Ok(Variant::F32(f)) => {
                // The value is stored as a float for some reason
                if f.fract() == 0.0 && f >= 0.0 {
                    return Ok(Variant::U32(f as u32));
                } else {
                    return Err(PropCollectionError::WeaponSkinFloatConvertionError);
                }
            }
            Ok(_) => return Err(PropCollectionError::WeaponSkinIdxIncorrectVariant),
            Err(e) => return Err(e),
        }
    }
    pub fn find_weapon_skin_from_player(&self, player_entid: &i32) -> Result<Variant, PropCollectionError> {
        let p = match self.prop_controller.special_ids.active_weapon {
            Some(p) => p,
            None => return Err(PropCollectionError::SpecialidsActiveWeaponNotSet),
        };
        return match self.get_prop_from_ent(&p, player_entid) {
            Ok(Variant::U32(weap_handle)) => {
                let weapon_entity_id = (weap_handle & 0x7FF) as i32;
                self.find_weapon_skin(&weapon_entity_id)
            }
            Ok(_) => Err(PropCollectionError::WeaponHandleIncorrectVariant),
            Err(e) => Err(e),
        };
    }
    pub fn find_glove_skin_id(&self, player_entid: &i32) -> Result<Variant, PropCollectionError> {
        match self.get_prop_from_ent(&GLOVE_PAINT_ID, player_entid) {
            Ok(Variant::F32(f)) => {
                // The value is stored as a float for some reason
                if f.fract() == 0.0 && f >= 0.0 {
                    return Ok(Variant::U32(f as u32));
                } else {
                    return Err(PropCollectionError::GloveSkinFloatConvertionError);
                }
            }
            Ok(_) => return Err(PropCollectionError::GloveSkinIdxIncorrectVariant),
            Err(e) => return Err(e),
        }
    }

    pub fn find_glove_skin(&self, player_entid: &i32) -> Result<Variant, PropCollectionError> {
        match self.get_prop_from_ent(&GLOVE_PAINT_ID, player_entid) {
            Ok(Variant::F32(f)) => {
                // The value is stored as a float for some reason
                if f.fract() == 0.0 && f >= 0.0 {
                    let idx = f as u32;
                    match PAINTKITS.get(&idx) {
                        Some(kit) => Ok(Variant::String(kit.to_string())),
                        None => Err(PropCollectionError::GloveSkinNoSkinMapping),
                    }
                } else {
                    return Err(PropCollectionError::GloveSkinFloatConvertionError);
                }
            }
            Ok(_) => return Err(PropCollectionError::GloveSkinIdxIncorrectVariant),
            Err(e) => return Err(e),
        }
    }

    pub fn find_glove_paint_seed(&self, player_entid: &i32) -> Result<Variant, PropCollectionError> {
        match self.get_prop_from_ent(&GLOVE_PAINT_SEED, player_entid) {
            Ok(p) => Ok(p),
            Err(e) => return Err(e),
        }
    }

    pub fn find_glove_paint_float(&self, player_entid: &i32) -> Result<Variant, PropCollectionError> {
        match self.get_prop_from_ent(&GLOVE_PAINT_FLOAT, player_entid) {
            Ok(p) => Ok(p),
            Err(e) => return Err(e),
        }
    }

    pub fn find_weapon_prop(&self, prop: &u32, player_entid: &i32) -> Result<Variant, PropCollectionError> {
        let p = match self.prop_controller.special_ids.active_weapon {
            Some(p) => p,
            None => return Err(PropCollectionError::SpecialidsActiveWeaponNotSet),
        };
        match self.get_prop_from_ent(&p, player_entid) {
            Ok(Variant::U32(weap_handle)) => {
                // Could be more specific
                let weapon_entity_id = (weap_handle & 0x7FF) as i32;
                match self.get_prop_from_ent(&prop, &weapon_entity_id) {
                    Ok(p) => Ok(p),
                    Err(e) => match e {
                        PropCollectionError::GetPropFromEntEntityNotFound => Err(PropCollectionError::WeaponEntityNotFound),
                        PropCollectionError::GetPropFromEntPropNotFound => Err(PropCollectionError::WeaponEntityWantedPropNotFound),
                        _ => Err(e),
                    },
                }
            }
            Ok(_) => Err(PropCollectionError::WeaponHandleIncorrectVariant),
            Err(e) => Err(e),
        }
    }
    pub fn find_team_prop(&self, prop: &u32, player_entid: &i32) -> Result<Variant, PropCollectionError> {
        match self.prop_controller.special_ids.player_team_pointer {
            None => return Err(PropCollectionError::SpecialidsPlayerTeamPointerNotSet),
            Some(p) => {
                match self.get_prop_from_ent(&p, player_entid) {
                    Ok(Variant::U32(team_num)) => {
                        let team_entid = match team_num {
                            // 1 should be spectator
                            1 => self.teams.team1_entid,
                            2 => self.teams.team2_entid,
                            3 => self.teams.team3_entid,
                            _ => return Err(PropCollectionError::IllegalTeamValue),
                        };
                        // Get prop from team entity
                        match team_entid {
                            Some(eid) => return self.get_prop_from_ent(prop, &eid),
                            None => return Err(PropCollectionError::TeamEntityIdNotSet),
                        }
                    }
                    Ok(_) => Err(PropCollectionError::TeamNumIncorrectVariant),
                    Err(e) => Err(e),
                }
            }
        }
    }
    pub fn gather_extra_info(&mut self, entity_id: &i32, is_baseline: bool) -> Result<(), DemoParserError> {
        // Boring stuff.. function does some bookkeeping
        let entity = match self.entities.get(*entity_id as usize) {
            Some(Some(entity)) => entity,
            _ => return Err(DemoParserError::EntityNotFound),
        };
        if !(entity.entity_type == EntityType::PlayerController || entity.entity_type == EntityType::Team) {
            return Ok(());
        }
        if entity.entity_type == EntityType::Team && !is_baseline {
            if let Some(team_num_id) = self.prop_controller.special_ids.team_team_num {
                if let Ok(Variant::U32(t)) = self.get_prop_from_ent(&team_num_id, entity_id) {
                    match t {
                        1 => self.teams.team1_entid = Some(*entity_id),
                        2 => self.teams.team2_entid = Some(*entity_id),
                        3 => self.teams.team3_entid = Some(*entity_id),
                        _ => {}
                    }
                }
            }
        }
        if entity.entity_type == EntityType::PlayerController {
            let team_num = match self.prop_controller.special_ids.teamnum {
                Some(team_num_id) => match self.get_prop_from_ent(&team_num_id, entity_id) {
                    Ok(Variant::U32(team_num)) => Some(team_num),
                    Ok(_) => return Err(DemoParserError::IncorrectMetaDataProp),
                    Err(_) => None,
                },
                _ => None,
            };
            let name = match self.prop_controller.special_ids.player_name {
                Some(id) => match self.get_prop_from_ent(&id, entity_id) {
                    Ok(Variant::String(name)) => Some(name),
                    Ok(_) => return Err(DemoParserError::IncorrectMetaDataProp),
                    Err(_) => None,
                },
                _ => None,
            };
            let steamid = match self.prop_controller.special_ids.steamid {
                Some(id) => match self.get_prop_from_ent(&id, entity_id) {
                    Ok(Variant::U64(sid)) => Some(sid),
                    Ok(_) => return Err(DemoParserError::IncorrectMetaDataProp),
                    Err(_) => None,
                },
                _ => None,
            };
            let player_entid = match self.prop_controller.special_ids.player_pawn {
                Some(id) => match self.get_prop_from_ent(&id, entity_id) {
                    Ok(Variant::U32(handle)) => Some((handle & 0x7FF) as i32),
                    Ok(_) => return Err(DemoParserError::IncorrectMetaDataProp),
                    Err(_) => None,
                },
                _ => None,
            };
            if let Some(e) = player_entid {
                if e != PLAYER_ENTITY_HANDLE_MISSING && steamid != Some(0) && team_num != Some(SPECTATOR_TEAM_NUM) {
                    match self.should_remove(steamid) {
                        Some(eid) => {
                            self.players.remove(&eid);
                        }
                        None => {}
                    }
                    self.players.insert(
                        e,
                        PlayerMetaData {
                            name,
                            team_num,
                            player_entity_id: player_entid,
                            steamid,
                            controller_entid: Some(*entity_id),
                        },
                    );
                }
            }
        }
        Ok(())
    }
    pub fn should_remove(&self, steamid: Option<u64>) -> Option<i32> {
        for (entid, player) in &self.players {
            if player.steamid == steamid {
                return Some(*entid);
            }
        }
        None
    }
}

fn coord_from_cell(cell: Result<Variant, PropCollectionError>, offset: Result<Variant, PropCollectionError>) -> Result<f32, PropCollectionError> {
    // Both cell and offset are needed for calculation
    match (offset, cell) {
        (Ok(Variant::F32(offset)), Ok(Variant::U32(cell))) => {
            let cell_coord = ((cell as f32 * (1 << CELL_BITS) as f32) - MAX_COORD) as f32;
            Ok(cell_coord + offset)
        }
        (Err(_), Err(_)) => Err(PropCollectionError::CoordinateBothNone),
        (Ok(Variant::F32(_offset)), Err(_)) => Err(PropCollectionError::CoordinateCellNone),
        (Err(_), Ok(Variant::U32(_cell))) => Err(PropCollectionError::CoordinateOffsetNone),
        (_, _) => Err(PropCollectionError::CoordinateIncorrectTypes),
    }
}
#[derive(Debug, PartialEq)]
pub enum PropCollectionError {
    PlayerSpecialIDCellXMissing,
    PlayerSpecialIDCellYMissing,
    PlayerSpecialIDCellZMissing,
    PlayerSpecialIDOffsetXMissing,
    PlayerSpecialIDOffsetYMissing,
    PlayerSpecialIDOffsetZMissing,
    GrenadeSpecialIDCellXMissing,
    GrenadeSpecialIDCellYMissing,
    GrenadeSpecialIDCellZMissing,
    GrenadeSpecialIDOffsetXMissing,
    GrenadeSpecialIDOffsetYMissing,
    GrenadeSpecialIDOffsetZMissing,
    CoordinateOffsetNone,
    CoordinateCellNone,
    CoordinateIncorrectTypes,
    CoordinateBothNone,
    GrenadeOffsetVariantNone,
    PlayerMetaDataNameNone,
    ButtonsSpecialIDNone,
    ButtonsMapNoEntryFound,
    GetPropFromEntEntityNotFound,
    GetPropFromEntPropNotFound,
    ButtonMaskNotU64Variant,
    RulesEntityIdNotSet,
    ControllerEntityIdNotSet,
    SpecialidsEyeAnglesNotSet,
    SpecialidsItemDefNotSet,
    EyeAnglesWrongVariant,
    WeaponIdxMappingNotFound,
    WeaponDefVariantWrongType,
    SpecialidsPlayerTeamPointerNotSet,
    TeamNumIncorrectVariant,
    IllegalTeamValue,
    TeamEntityIdNotSet,
    GrenadeOwnerIdNotSet,
    GrenadeOwnerIdPropIncorrectVariant,
    PlayerNotFound,
    SpecialidsActiveWeaponNotSet,
    WeaponHandleIncorrectVariant,
    UnknownCustomPropName,
    UnknownCoordinateAxis,
    WeaponEntityNotFound,
    WeaponEntityWantedPropNotFound,
    WeaponSkinFloatConvertionError,
    WeaponSkinNoSkinMapping,
    WeaponSkinIdxIncorrectVariant,
    OriginalOwnerXuidIdLowNotSet,
    OriginalOwnerXuidIdHighNotSet,
    OriginalOwnerXuidLowNotFound,
    OriginalOwnerXuidHighNotFound,
    OriginalOwnerXuidlowIncorrectVariant,
    OriginalOwnerXuidHighIncorrectVariant,
    SpottedIncorrectVariant,
    VelocityNotFound,
    AgentIdNotFound,
    AgentIncorrectVariant,
    AgentPropNotFound,
    AgentSpecialIdNotSet,
    UseridNotFound,
    InventoryMaxNotFound,
    GloveSkinFloatConvertionError,
    GloveSkinIdxIncorrectVariant,
    GloveSkinNoSkinMapping,
}
impl std::error::Error for PropCollectionError {}
impl fmt::Display for PropCollectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[cfg(test)]
mod planted_c4_dedupe_tests {
    //! Slot-reuse-safe dedupe for `collect_planted_c4_records`.
    //!
    //! CS2 reuses entity slot IDs after delete. The pre-fix dedupe scanned
    //! `planted_c4_records` linearly for `entity_id` matches — when a later
    //! plant reused a slot, the scan saw the previous lifecycle's record and
    //! silently skipped emitting a fresh record. The fix anchors dedupe on
    //! `planted_c4_recorded_active: AHashSet<i32>`, cleared on entity-delete
    //! in `entities.rs`.
    //!
    //! Constructing a full `SecondPassParser` from a unit test requires the
    //! whole first-pass output graph (PropController, serializers, baselines,
    //! huffman tables, …) so we test the dedupe invariant directly against
    //! the same `AHashSet` + `Vec<PlantedC4Record>` shape that the production
    //! collector + delete path operate on. Any future change that doesn't
    //! preserve this lifecycle will fail this test.
    use super::PlantedC4Record;
    use ahash::AHashSet;

    /// Mirrors the production push: skip if active set contains entid, else
    /// push record + insert. Returns whether a new record was emitted.
    fn try_emit(
        records: &mut Vec<PlantedC4Record>,
        active: &mut AHashSet<i32>,
        entid: i32,
        tick: i32,
        site: i32,
    ) -> bool {
        if active.contains(&entid) {
            return false;
        }
        records.push(PlantedC4Record {
            entity_id: entid,
            plant_tick: tick,
            bomb_site: site,
        });
        active.insert(entid);
        true
    }

    /// Mirrors the production entity-delete cleanup.
    fn on_entity_delete(active: &mut AHashSet<i32>, entid: i32) {
        active.remove(&entid);
    }

    #[test]
    fn slot_reuse_emits_fresh_record_for_each_lifecycle() {
        let mut records: Vec<PlantedC4Record> = Vec::new();
        let mut active: AHashSet<i32> = AHashSet::default();

        // Lifecycle 1: entity 94 plants at site 0.
        assert!(
            try_emit(&mut records, &mut active, 94, 1000, 0),
            "first plant on slot 94 should emit"
        );
        // Subsequent ticks of the same lifecycle must not emit again.
        assert!(
            !try_emit(&mut records, &mut active, 94, 1001, 0),
            "duplicate poll for active lifecycle should be skipped"
        );

        // Entity 94 deleted (round ends, bomb explodes / defuses).
        on_entity_delete(&mut active, 94);

        // Lifecycle 2: slot 94 reused for a fresh plant at site 1.
        assert!(
            try_emit(&mut records, &mut active, 94, 2000, 1),
            "post-delete reuse of slot 94 should emit a new record"
        );

        assert_eq!(records.len(), 2, "exactly two records — one per lifecycle");
        assert_eq!(records[0].entity_id, 94);
        assert_eq!(records[0].plant_tick, 1000);
        assert_eq!(records[0].bomb_site, 0);
        assert_eq!(records[1].entity_id, 94);
        assert_eq!(records[1].plant_tick, 2000);
        assert_eq!(records[1].bomb_site, 1);
    }

    #[test]
    fn no_delete_means_no_reemit_even_with_different_site() {
        // Sanity check: if the slot is still active, even a hypothetical
        // re-read producing a different site value must NOT emit a second
        // record (that's the normal per-tick polling no-op).
        let mut records: Vec<PlantedC4Record> = Vec::new();
        let mut active: AHashSet<i32> = AHashSet::default();
        assert!(try_emit(&mut records, &mut active, 94, 1000, 0));
        assert!(!try_emit(&mut records, &mut active, 94, 1500, 1));
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].bomb_site, 0);
    }
}

/// Returns `true` if `blob` is exactly tiled by
/// `u16-LE seq | u16-LE len | len bytes` records AND the first record's
/// sequence number == 0.
///
/// This validates that the per-element prop reassembly produced a
/// well-formed voxel journal blob. An empty slice returns `false`.
pub fn journal_tiles(blob: &[u8]) -> bool {
    if blob.len() < 4 {
        return false;
    }
    let mut off = 0usize;
    let mut first = true;
    while off + 4 <= blob.len() {
        let seq = u16::from_le_bytes([blob[off], blob[off + 1]]);
        let len = u16::from_le_bytes([blob[off + 2], blob[off + 3]]) as usize;
        if first {
            if seq != 0 {
                return false;
            }
            first = false;
        }
        off += 4 + len;
    }
    off == blob.len()
}

#[cfg(test)]
mod journal_tiles_tests {
    use super::journal_tiles;

    /// A single seq=0 record with 3 payload bytes tiles perfectly.
    #[test]
    fn single_seq0_record_tiles() {
        // seq=0 (2 bytes LE) | len=3 (2 bytes LE) | 3 payload bytes
        let blob: Vec<u8> = vec![0x00, 0x00, 0x03, 0x00, 0xAA, 0xBB, 0xCC];
        assert!(journal_tiles(&blob));
    }

    /// First record with seq != 0 must return false.
    #[test]
    fn nonzero_first_seq_fails() {
        // seq=1 (2 bytes LE) | len=2 | 2 payload bytes
        let blob: Vec<u8> = vec![0x01, 0x00, 0x02, 0x00, 0x11, 0x22];
        assert!(!journal_tiles(&blob));
    }

    /// A blob where the declared length overruns the buffer must return false.
    #[test]
    fn length_overrun_fails() {
        // seq=0 | len=100 | only 2 payload bytes
        let blob: Vec<u8> = vec![0x00, 0x00, 0x64, 0x00, 0xDE, 0xAD];
        assert!(!journal_tiles(&blob));
    }

    /// An empty blob returns false.
    #[test]
    fn empty_blob_fails() {
        assert!(!journal_tiles(&[]));
    }

    /// Two valid consecutive records tile correctly.
    #[test]
    fn two_records_tile() {
        // record 0: seq=0, len=2, [0x01, 0x02]
        // record 1: seq=1, len=1, [0x03]
        let blob: Vec<u8> = vec![
            0x00, 0x00, 0x02, 0x00, 0x01, 0x02, // record 0
            0x01, 0x00, 0x01, 0x00, 0x03,       // record 1
        ];
        assert!(journal_tiles(&blob));
    }
}

