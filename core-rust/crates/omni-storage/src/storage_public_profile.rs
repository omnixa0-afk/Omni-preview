use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;

const PUBLIC_PROFILE_AVATAR_TARGET_DIMENSION: u32 = 256;
const PUBLIC_PROFILE_AVATAR_MAX_INPUT_BYTES: u64 = 20 * 1024 * 1024;
const PUBLIC_PROFILE_AVATAR_MAX_INPUT_PIXELS: u64 = 16_000_000;
const PUBLIC_PROFILE_AVATAR_JPEG_QUALITY: u8 = 82;

impl Storage {
    pub(crate) fn hydrate_contact_public_avatar_image_path(
        &self,
        profile_id: &LocalProfileId,
        mut contact: ContactSnapshot,
    ) -> ContactSnapshot {
        contact.public_avatar_image_path = self.resolve_contact_public_avatar_image_path(
            profile_id,
            contact.remote_account_id.as_ref().map(|value| value.0.as_str()),
            contact.public_avatar_hash_hex.as_deref(),
        );
        contact
    }

    fn profile_media_root_dir(&self) -> OmniResult<PathBuf> {
        let root = Self::profile_media_root_for_db_path(&self.db_path)?;
        fs::create_dir_all(&root).map_err(|error| {
            OmniError::Storage(format!(
                "failed to create profile media root '{}': {error}",
                root.display()
            ))
        })?;
        Ok(root)
    }

    fn profile_media_profile_dir(&self, profile_id: &LocalProfileId) -> OmniResult<PathBuf> {
        let path = self
            .profile_media_root_dir()?
            .join("p")
            .join(compact_avatar_storage_key(&profile_id.0));
        fs::create_dir_all(&path).map_err(|error| {
            OmniError::Storage(format!(
                "failed to create profile media directory '{}': {error}",
                path.display()
            ))
        })?;
        Ok(path)
    }

    fn legacy_profile_media_profile_dir(&self, profile_id: &LocalProfileId) -> OmniResult<PathBuf> {
        let path = self.profile_media_root_dir()?.join(&profile_id.0);
        fs::create_dir_all(&path).map_err(|error| {
            OmniError::Storage(format!(
                "failed to create profile media directory '{}': {error}",
                path.display()
            ))
        })?;
        Ok(path)
    }

    fn local_public_avatar_path_for_profile(
        &self,
        profile_id: &LocalProfileId,
        avatar_hash_hex: &str,
    ) -> OmniResult<PathBuf> {
        let avatars_dir = self.profile_media_profile_dir(profile_id)?.join("local");
        fs::create_dir_all(&avatars_dir).map_err(|error| {
            OmniError::Storage(format!(
                "failed to create local avatar directory '{}': {error}",
                avatars_dir.display()
            ))
        })?;
        Ok(avatars_dir.join(format!(
            "{}.jpg",
            avatar_hash_hex.to_ascii_lowercase()
        )))
    }

    fn legacy_local_public_avatar_path_for_profile(
        &self,
        profile_id: &LocalProfileId,
        avatar_hash_hex: &str,
    ) -> OmniResult<PathBuf> {
        let avatars_dir = self.legacy_profile_media_profile_dir(profile_id)?.join("local");
        fs::create_dir_all(&avatars_dir).map_err(|error| {
            OmniError::Storage(format!(
                "failed to create local avatar directory '{}': {error}",
                avatars_dir.display()
            ))
        })?;
        Ok(avatars_dir.join(format!(
            "{}.jpg",
            avatar_hash_hex.to_ascii_lowercase()
        )))
    }

    fn group_avatar_path_for_profile(&self, profile_id: &LocalProfileId, avatar_hash_hex: &str) -> OmniResult<PathBuf> {
        let groups_dir = self.profile_media_profile_dir(profile_id)?.join("groups");
        fs::create_dir_all(&groups_dir).map_err(|error| {
            OmniError::Storage(format!(
                "failed to create group avatar directory '{}': {error}",
                groups_dir.display()
            ))
        })?;
        Ok(groups_dir.join(format!("{}.jpg", sanitize_public_profile_path_component(&avatar_hash_hex.to_ascii_lowercase()))))
    }

    fn member_avatar_path_for_profile(&self, profile_id: &LocalProfileId, avatar_hash_hex: &str) -> OmniResult<PathBuf> {
        let members_dir = self.profile_media_profile_dir(profile_id)?.join("members");
        fs::create_dir_all(&members_dir).map_err(|error| {
            OmniError::Storage(format!(
                "failed to create group member avatar directory '{}': {error}",
                members_dir.display()
            ))
        })?;
        Ok(members_dir.join(format!("{}.jpg", sanitize_public_profile_path_component(&avatar_hash_hex.to_ascii_lowercase()))))
    }

    /// Keep what a group member said about itself, if it is newer than what we have
    /// (docs/decisions/group-member-profile.md). Returns whether anything changed.
    pub fn store_group_member_profile(
        &self,
        account_id: &str,
        version: i64,
        display_name: &str,
        avatar_jpeg: Option<&[u8]>,
        now_unix_ms: i64,
    ) -> OmniResult<bool> {
        let profile_id = self.require_active_profile_id()?;
        let known: Option<(i64, Option<String>)> = self
            .connection
            .query_row(
                "SELECT version, avatar_hash FROM group_member_profiles WHERE profile_id = ?1 AND account_id = ?2",
                params![profile_id.0, account_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        if known.as_ref().is_some_and(|(known_version, _)| *known_version >= version) {
            return Ok(false);
        }
        let hash = match avatar_jpeg {
            Some(bytes) => {
                let hash = hex::encode(sha2::Sha256::digest(bytes));
                let path = self.member_avatar_path_for_profile(&profile_id, &hash)?;
                if !path.exists() {
                    self.write_avatar_file(&path, bytes)?;
                }
                Some(hash)
            }
            None => None,
        };
        if let Some((_, Some(old_hash))) = &known {
            if hash.as_ref() != Some(old_hash) {
                let _ = fs::remove_file(self.member_avatar_path_for_profile(&profile_id, old_hash)?);
            }
        }
        self.connection
            .execute(
                "INSERT INTO group_member_profiles (profile_id, account_id, version, display_name, avatar_hash, updated_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(profile_id, account_id) DO UPDATE SET
                    version = excluded.version, display_name = excluded.display_name,
                    avatar_hash = excluded.avatar_hash, updated_at_unix_ms = excluded.updated_at_unix_ms",
                params![profile_id.0, account_id, version, display_name, hash, now_unix_ms],
            )
            .map_err(map_sqlite_error)?;
        Ok(true)
    }

    /// What a group member told the group about itself: name and the photo file, if we have one.
    pub fn get_group_member_profile(&self, account_id: &str) -> OmniResult<Option<GroupMemberProfileRecord>> {
        let profile_id = self.require_active_profile_id()?;
        let row: Option<(i64, String, Option<String>)> = self
            .connection
            .query_row(
                "SELECT version, display_name, avatar_hash FROM group_member_profiles WHERE profile_id = ?1 AND account_id = ?2",
                params![profile_id.0, account_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let Some((version, display_name, avatar_hash)) = row else {
            return Ok(None);
        };
        let avatar_path = match avatar_hash {
            Some(hash) => {
                let path = self.member_avatar_path_for_profile(&profile_id, &hash)?;
                path.exists().then(|| path.to_string_lossy().to_string())
            }
            None => None,
        };
        Ok(Some(GroupMemberProfileRecord { account_id: account_id.to_string(), version, display_name, avatar_path }))
    }

    /// Forget the profiles of people who are in none of our groups any more (and their photos).
    pub fn forget_group_member_profiles_outside_groups(&self) -> OmniResult<usize> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT account_id, avatar_hash FROM group_member_profiles p WHERE p.profile_id = ?1
                 AND NOT EXISTS (SELECT 1 FROM group_members m WHERE m.profile_id = p.profile_id
                                 AND m.account_id = p.account_id AND m.status = 'member')",
            )
            .map_err(map_sqlite_error)?;
        let gone: Vec<(String, Option<String>)> = statement
            .query_map(params![profile_id.0], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(map_sqlite_error)?
            .collect::<Result<_, _>>()
            .map_err(map_sqlite_error)?;
        for (account_id, avatar_hash) in &gone {
            if let Some(hash) = avatar_hash {
                let _ = fs::remove_file(self.member_avatar_path_for_profile(&profile_id, hash)?);
            }
            self.connection
                .execute(
                    "DELETE FROM group_member_profiles WHERE profile_id = ?1 AND account_id = ?2",
                    params![profile_id.0, account_id],
                )
                .map_err(map_sqlite_error)?;
        }
        Ok(gone.len())
    }

    /// Keep a group's picture (content-addressed by its sha256) and point the group at it; `None`
    /// clears it. The file is the shell's to show (`group_avatar_image_path`).
    pub fn set_group_avatar(&self, group_id: &str, jpeg: Option<&[u8]>) -> OmniResult<Option<String>> {
        let profile_id = self.require_active_profile_id()?;
        let hash = match jpeg {
            Some(bytes) => {
                let hash = hex::encode(sha2::Sha256::digest(bytes));
                let path = self.group_avatar_path_for_profile(&profile_id, &hash)?;
                if !path.exists() {
                    self.write_avatar_file(&path, bytes)?;
                }
                Some(hash)
            }
            None => None,
        };
        self.connection
            .execute(
                "UPDATE groups SET avatar_hash = ?3 WHERE profile_id = ?1 AND group_id = ?2",
                params![profile_id.0, group_id, hash],
            )
            .map_err(map_sqlite_error)?;
        Ok(hash)
    }

    pub fn get_group_avatar_hash(&self, group_id: &str) -> OmniResult<Option<String>> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .query_row(
                "SELECT avatar_hash FROM groups WHERE profile_id = ?1 AND group_id = ?2",
                params![profile_id.0, group_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(Option::flatten)
            .map_err(map_sqlite_error)
    }

    /// The file of a group's picture, if it has one and the file is there.
    pub fn group_avatar_image_path(&self, group_id: &str) -> OmniResult<Option<String>> {
        let Some(hash) = self.get_group_avatar_hash(group_id)? else {
            return Ok(None);
        };
        let profile_id = self.require_active_profile_id()?;
        let path = self.group_avatar_path_for_profile(&profile_id, &hash)?;
        Ok(path.exists().then(|| path.to_string_lossy().to_string()))
    }

    fn contact_public_avatar_path_for_profile(
        &self,
        profile_id: &LocalProfileId,
        avatar_hash_hex: &str,
    ) -> OmniResult<PathBuf> {
        let contacts_dir = self.profile_media_profile_dir(profile_id)?.join("contacts");
        fs::create_dir_all(&contacts_dir).map_err(|error| {
            OmniError::Storage(format!(
                "failed to create contact avatar directory '{}': {error}",
                contacts_dir.display()
            ))
        })?;
        Ok(contacts_dir.join(format!(
            "{}.jpg",
            avatar_hash_hex.to_ascii_lowercase()
        )))
    }

    fn legacy_contact_public_avatar_path_for_profile(
        &self,
        profile_id: &LocalProfileId,
        remote_account_id: &str,
        avatar_hash_hex: &str,
    ) -> OmniResult<PathBuf> {
        let contacts_dir = self.legacy_profile_media_profile_dir(profile_id)?.join("contacts");
        fs::create_dir_all(&contacts_dir).map_err(|error| {
            OmniError::Storage(format!(
                "failed to create contact avatar directory '{}': {error}",
                contacts_dir.display()
            ))
        })?;
        Ok(contacts_dir.join(format!(
            "{}-{}.jpg",
            sanitize_public_profile_path_component(remote_account_id),
            avatar_hash_hex.to_ascii_lowercase()
        )))
    }

    pub(crate) fn resolve_local_public_avatar_image_path(
        &self,
        profile_id: &LocalProfileId,
        avatar_hash_hex: Option<&str>,
    ) -> Option<String> {
        let _hash = avatar_hash_hex.filter(|value| !value.trim().is_empty())?;
        let new_path = self
            .local_public_avatar_path_for_profile(profile_id, _hash)
            .ok()?;
        if new_path.exists() {
            return Some(new_path.to_string_lossy().to_string());
        }

        let legacy_path = self
            .legacy_local_public_avatar_path_for_profile(profile_id, _hash)
            .ok()?;
        legacy_path
            .exists()
            .then(|| legacy_path.to_string_lossy().to_string())
    }

    pub(crate) fn resolve_contact_public_avatar_image_path(
        &self,
        profile_id: &LocalProfileId,
        remote_account_id: Option<&str>,
        avatar_hash_hex: Option<&str>,
    ) -> Option<String> {
        let remote_account_id = remote_account_id.filter(|value| !value.trim().is_empty())?;
        let avatar_hash_hex = avatar_hash_hex.filter(|value| !value.trim().is_empty())?;
        let new_path = self
            .contact_public_avatar_path_for_profile(profile_id, avatar_hash_hex)
            .ok()?;
        if new_path.exists() {
            return Some(new_path.to_string_lossy().to_string());
        }

        let legacy_path = self
            .legacy_contact_public_avatar_path_for_profile(
                profile_id,
                remote_account_id,
                avatar_hash_hex,
            )
            .ok()?;
        legacy_path
            .exists()
            .then(|| legacy_path.to_string_lossy().to_string())
    }

    pub fn get_local_public_profile_state(&self) -> OmniResult<LocalPublicProfileState> {
        let profile_id = self.require_active_profile_id()?;
        let account = self
            .get_local_account_by_profile_id(&profile_id)?
            .ok_or_else(|| OmniError::Storage("local account is not available".to_string()))?;

        let avatar_bytes = self
            .resolve_local_public_avatar_image_path(
                &profile_id,
                account.public_avatar_hash_hex.as_deref(),
            )
            .and_then(|path| self.read_media_file(&std::path::PathBuf::from(path)).ok());
        let (avatar_hash_hex, avatar_mime_type) = if avatar_bytes.is_some() {
            (
                account.public_avatar_hash_hex.clone(),
                account.public_avatar_mime_type.clone(),
            )
        } else {
            (None, None)
        };

        Ok(LocalPublicProfileState {
            display_name: account.descriptor.display_name,
            avatar_hash_hex,
            avatar_mime_type,
            avatar_bytes,
            updated_at_unix_ms: account
                .public_profile_updated_at_unix_ms
                .unwrap_or(account.updated_at_unix_ms),
        })
    }

    pub fn update_local_public_profile(
        &self,
        display_name: &str,
        avatar_source_path: Option<&Path>,
        clear_avatar: bool,
        updated_at_unix_ms: i64,
    ) -> OmniResult<LocalAccountSnapshot> {
        let profile_id = self.require_active_profile_id()?;
        let current = self
            .get_local_account_by_profile_id(&profile_id)?
            .ok_or_else(|| OmniError::Storage("local account is not available".to_string()))?;
        let normalized_display_name = validate_non_empty_public_profile_display_name(display_name)?;
        let avatar_update = if clear_avatar {
            None
        } else if let Some(source_path) = avatar_source_path {
            Some(normalize_public_avatar_image(source_path)?)
        } else {
            None
        };
        let next_avatar_hash_hex = match &avatar_update {
            Some(avatar) => Some(avatar.sha256_hex.clone()),
            None if clear_avatar => None,
            None => current.public_avatar_hash_hex.clone(),
        };
        let next_avatar_mime_type = match &avatar_update {
            Some(avatar) => Some(avatar.mime_type.clone()),
            None if clear_avatar => None,
            None => current.public_avatar_mime_type.clone(),
        };
        if normalized_display_name == current.descriptor.display_name
            && next_avatar_hash_hex == current.public_avatar_hash_hex
            && next_avatar_mime_type == current.public_avatar_mime_type
        {
            return Ok(current);
        }

        let previous_avatar_path = self
            .resolve_local_public_avatar_image_path(
                &profile_id,
                current.public_avatar_hash_hex.as_deref(),
            )
            .map(PathBuf::from);
        let next_avatar_path = avatar_update.as_ref().and_then(|avatar| {
            self.local_public_avatar_path_for_profile(&profile_id, &avatar.sha256_hex)
                .ok()
        });
        let mut created_new_avatar_file = false;

        if let (Some(avatar), Some(next_avatar_path)) = (&avatar_update, next_avatar_path.as_ref()) {
            if previous_avatar_path.as_ref() != Some(next_avatar_path) || !next_avatar_path.exists() {
                self.write_avatar_file(next_avatar_path, &avatar.bytes)?;
                created_new_avatar_file = true;
            }
        }

        let mutation_result = self.with_immediate_transaction("update_local_public_profile", |storage| {
            storage.write_local_public_profile(
                &profile_id,
                &normalized_display_name,
                next_avatar_hash_hex.as_deref(),
                next_avatar_mime_type.as_deref(),
                updated_at_unix_ms,
            )
        });

        if let Err(error) = mutation_result {
            if created_new_avatar_file {
                if let Some(next_avatar_path) = next_avatar_path.as_ref() {
                    let _ = best_effort_remove_file(next_avatar_path);
                }
            }
            return Err(error);
        }

        if current.public_avatar_hash_hex != next_avatar_hash_hex {
            if let Some(previous_avatar_path) = previous_avatar_path.as_ref() {
                let _ = best_effort_remove_file(previous_avatar_path);
            }
        }

        self.get_local_account_by_profile_id(&profile_id)?
            .ok_or_else(|| OmniError::Storage("local account is missing after update".to_string()))
    }

    /// The profile's public fields, in both rows that carry them. `updated_at_unix_ms` is the moment
    /// the profile was changed — on whichever device of the account changed it.
    fn write_local_public_profile(
        &self,
        profile_id: &LocalProfileId,
        display_name: &str,
        avatar_hash_hex: Option<&str>,
        avatar_mime_type: Option<&str>,
        updated_at_unix_ms: i64,
    ) -> OmniResult<()> {
        for table in ["profile_registry", "local_account"] {
            self.connection
                .execute(
                    &format!(
                        "
                        UPDATE {table}
                        SET display_name = ?2,
                            public_avatar_hash_hex = ?3,
                            public_avatar_mime_type = ?4,
                            public_profile_updated_at_unix_ms = ?5,
                            updated_at_unix_ms = ?6
                        WHERE profile_id = ?1
                        "
                    ),
                    params![
                        profile_id.0.as_str(),
                        display_name,
                        avatar_hash_hex,
                        avatar_mime_type,
                        updated_at_unix_ms,
                        updated_at_unix_ms,
                    ],
                )
                .map_err(map_sqlite_error)?;
        }
        Ok(())
    }

    /// Another device of our account changed the profile (docs/decisions/multi-device-equal-nodes.md,
    /// «Синхронизация профиля»): take its name — and its avatar when it hands the picture over —
    /// if its change is newer than ours. The change keeps the moment it was made, so passing it on
    /// does not make it newer and two devices never bounce it back and forth.
    ///
    /// The name comes with a descriptor (`avatar_bytes` = `None`): a descriptor without an avatar
    /// clears ours; one with a different avatar leaves ours until its picture arrives. The picture
    /// comes on its own (`avatar_bytes` = `Some`) with the same moment. Returns whether anything
    /// changed.
    pub fn adopt_public_profile_from_own_device(
        &self,
        display_name: Option<&str>,
        avatar_hash_hex: Option<&str>,
        avatar_mime_type: Option<&str>,
        avatar_bytes: Option<&[u8]>,
        updated_at_unix_ms: i64,
    ) -> OmniResult<bool> {
        let profile_id = self.require_active_profile_id()?;
        let current = self
            .get_local_account_by_profile_id(&profile_id)?
            .ok_or_else(|| OmniError::Storage("local account is not available".to_string()))?;
        let current_updated_at = current
            .public_profile_updated_at_unix_ms
            .unwrap_or(current.updated_at_unix_ms);
        if updated_at_unix_ms < current_updated_at {
            return Ok(false);
        }
        let incoming_hash = avatar_hash_hex
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_ascii_lowercase);
        let incoming_mime = avatar_mime_type
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);

        let mut next_name = current.descriptor.display_name.clone();
        let mut next_hash = current.public_avatar_hash_hex.clone();
        let mut next_mime = current.public_avatar_mime_type.clone();
        let mut new_avatar_path = None;
        match avatar_bytes {
            None => {
                if let Some(name) = display_name {
                    next_name = validate_non_empty_public_profile_display_name(name)?;
                }
                if incoming_hash.is_none() {
                    next_hash = None;
                    next_mime = None;
                }
            }
            Some(bytes) => {
                let Some(expected_hash) = incoming_hash.as_deref() else {
                    return Err(OmniError::InvalidInput("own device avatar hash is missing".to_string()));
                };
                if hex::encode(sha2::Sha256::digest(bytes)) != expected_hash {
                    return Err(OmniError::InvalidInput(
                        "own device avatar hash does not match the payload".to_string(),
                    ));
                }
                if next_hash.as_deref() != Some(expected_hash) {
                    let path = self.local_public_avatar_path_for_profile(&profile_id, expected_hash)?;
                    self.write_avatar_file(&path, bytes)?;
                    new_avatar_path = Some(path);
                    next_hash = incoming_hash.clone();
                    next_mime = incoming_mime;
                }
            }
        }
        if next_name == current.descriptor.display_name
            && next_hash == current.public_avatar_hash_hex
            && next_mime == current.public_avatar_mime_type
            && updated_at_unix_ms == current_updated_at
        {
            return Ok(false);
        }

        let previous_avatar_path = self
            .resolve_local_public_avatar_image_path(&profile_id, current.public_avatar_hash_hex.as_deref())
            .map(PathBuf::from);
        let written = self.with_immediate_transaction("adopt_public_profile_from_own_device", |storage| {
            storage.write_local_public_profile(
                &profile_id,
                &next_name,
                next_hash.as_deref(),
                next_mime.as_deref(),
                updated_at_unix_ms,
            )
        });
        if let Err(error) = written {
            if let Some(path) = new_avatar_path.as_ref() {
                let _ = best_effort_remove_file(path);
            }
            return Err(error);
        }
        if current.public_avatar_hash_hex != next_hash {
            if let Some(path) = previous_avatar_path.as_ref() {
                let _ = best_effort_remove_file(path);
            }
        }
        Ok(true)
    }

    pub fn apply_remote_public_profile_descriptor(
        &self,
        remote_account_id: Option<&AccountId>,
        remote_instance_id: Option<&str>,
        display_name: &str,
        avatar_hash_hex: Option<&str>,
        avatar_mime_type: Option<&str>,
        public_profile_updated_at_unix_ms: i64,
    ) -> OmniResult<Option<ContactSnapshot>> {
        let profile_id = self.require_active_profile_id()?;
        let contact = if let Some(account_id) = remote_account_id {
            self.find_contact_by_remote_account_id(account_id)?
        } else if let Some(instance_id) = remote_instance_id {
            self.find_contact_by_remote_instance_id(instance_id)?
        } else {
            None
        };
        let Some(contact) = contact else {
            return Ok(None);
        };

        let normalized_display_name =
            validate_non_empty_public_profile_display_name(display_name)?;
        let current_profile_updated_at = contact.public_profile_updated_at_unix_ms.unwrap_or(0);
        if current_profile_updated_at > public_profile_updated_at_unix_ms {
            return Ok(Some(contact));
        }

        let next_avatar_hash_hex = avatar_hash_hex
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_ascii_lowercase);
        let next_avatar_mime_type = avatar_mime_type
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let current_avatar_path = self.resolve_contact_public_avatar_image_path(
            &profile_id,
            contact.remote_account_id.as_ref().map(|value| value.0.as_str()),
            contact.public_avatar_hash_hex.as_deref(),
        );
        let next_avatar_is_available = next_avatar_hash_hex.as_deref().is_some_and(|hash_hex| {
            contact.public_avatar_hash_hex.as_deref() == Some(hash_hex)
                && current_avatar_path.is_some()
        });
        let next_public_avatar_hash_hex = if next_avatar_hash_hex.is_none() {
            None
        } else if next_avatar_is_available {
            next_avatar_hash_hex.clone()
        } else {
            contact.public_avatar_hash_hex.clone()
        };
        let next_public_avatar_mime_type = if next_avatar_hash_hex.is_none() {
            None
        } else if next_avatar_is_available {
            next_avatar_mime_type.clone()
        } else {
            contact.public_avatar_mime_type.clone()
        };
        let next_public_avatar_pending_hash_hex = if next_avatar_is_available {
            None
        } else {
            next_avatar_hash_hex.clone()
        };
        let next_public_avatar_pending_mime_type = if next_avatar_is_available {
            None
        } else {
            next_avatar_mime_type.clone()
        };

        let current_now = current_unix_ms();
        let updated_rows = self
            .connection
            .execute(
                "
                UPDATE contacts
                SET public_display_name = ?2,
                    public_avatar_hash_hex = ?3,
                    public_avatar_mime_type = ?4,
                    public_avatar_pending_hash_hex = ?5,
                    public_avatar_pending_mime_type = ?6,
                    public_profile_updated_at_unix_ms = ?7,
                    updated_at_unix_ms = ?8
                WHERE contact_id = ?1
                  AND profile_id = ?9
                ",
                params![
                    contact.contact_id.0.as_str(),
                    normalized_display_name,
                    next_public_avatar_hash_hex.as_deref(),
                    next_public_avatar_mime_type.as_deref(),
                    next_public_avatar_pending_hash_hex.as_deref(),
                    next_public_avatar_pending_mime_type.as_deref(),
                    public_profile_updated_at_unix_ms,
                    current_now,
                    profile_id.0.as_str(),
                ],
            )
            .map_err(map_sqlite_error)?;
        if updated_rows == 0 {
            return Ok(None);
        }

        let _ = self.connection.execute(
            "
            UPDATE contact_identity_observations
            SET observed_display_name = ?2,
                updated_at_unix_ms = ?3
            WHERE contact_id = ?1
              AND profile_id = ?4
            ",
            params![
                contact.contact_id.0.as_str(),
                normalized_display_name,
                current_now,
                profile_id.0.as_str(),
            ],
        );

        self.get_contact(&contact.contact_id).map(Some)
    }

    pub fn apply_remote_public_profile_avatar(
        &self,
        remote_account_id: Option<&AccountId>,
        remote_instance_id: Option<&str>,
        avatar_hash_hex: Option<&str>,
        avatar_mime_type: Option<&str>,
        avatar_bytes: Option<&[u8]>,
        public_profile_updated_at_unix_ms: i64,
    ) -> OmniResult<Option<ContactSnapshot>> {
        let profile_id = self.require_active_profile_id()?;
        let contact = if let Some(account_id) = remote_account_id {
            self.find_contact_by_remote_account_id(account_id)?
        } else if let Some(instance_id) = remote_instance_id {
            self.find_contact_by_remote_instance_id(instance_id)?
        } else {
            None
        };
        let Some(contact) = contact else {
            return Ok(None);
        };

        let current_profile_updated_at = contact.public_profile_updated_at_unix_ms.unwrap_or(0);
        if current_profile_updated_at > public_profile_updated_at_unix_ms {
            return Ok(Some(contact));
        }

        let next_avatar_hash_hex = avatar_hash_hex
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_ascii_lowercase);
        let next_avatar_mime_type = avatar_mime_type
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let Some(expected_hash_hex) = next_avatar_hash_hex.as_deref() else {
            return Err(OmniError::InvalidInput(
                "remote public profile avatar hash is missing".to_string(),
            ));
        };
        let Some(bytes) = avatar_bytes else {
            return Err(OmniError::InvalidInput(
                "remote public profile avatar payload is missing".to_string(),
            ));
        };
        let actual_hash_hex = hex::encode(sha2::Sha256::digest(bytes));
        if actual_hash_hex != expected_hash_hex {
            return Err(OmniError::InvalidInput(
                "remote public profile avatar hash does not match the payload".to_string(),
            ));
        }
        let Some(_account_id) = contact.remote_account_id.as_ref() else {
            return Err(OmniError::InvalidInput(
                "remote public profile avatar cannot be resolved without a remote account id"
                    .to_string(),
            ));
        };
        let new_avatar_path =
            self.contact_public_avatar_path_for_profile(&profile_id, expected_hash_hex)?;
        self.write_avatar_file(&new_avatar_path, bytes)?;

        let current_now = current_unix_ms();
        let updated_rows = self
            .connection
            .execute(
                "
                UPDATE contacts
                SET public_avatar_hash_hex = ?2,
                    public_avatar_mime_type = ?3,
                    public_avatar_pending_hash_hex = NULL,
                    public_avatar_pending_mime_type = NULL,
                    public_profile_updated_at_unix_ms = ?4,
                    updated_at_unix_ms = ?5
                WHERE contact_id = ?1
                  AND profile_id = ?6
                ",
                params![
                    contact.contact_id.0.as_str(),
                    next_avatar_hash_hex.as_deref(),
                    next_avatar_mime_type.as_deref(),
                    public_profile_updated_at_unix_ms,
                    current_now,
                    profile_id.0.as_str(),
                ],
            )
            .map_err(map_sqlite_error)?;
        if updated_rows == 0 {
            let _ = best_effort_remove_file(&new_avatar_path);
            return Ok(None);
        }

        self.get_contact(&contact.contact_id).map(Some)
    }

    pub fn delete_public_profile_assets_for_profile(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<()> {
        for profile_dir in [
            self.profile_media_profile_dir(profile_id)?,
            self.legacy_profile_media_profile_dir(profile_id)?,
        ] {
            if !profile_dir.exists() {
                continue;
            }
            fs::remove_dir_all(&profile_dir).map_err(|error| {
                OmniError::Storage(format!(
                    "failed to delete profile media directory '{}': {error}",
                    profile_dir.display()
                ))
            })?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct NormalizedPublicAvatarImage {
    bytes: Vec<u8>,
    sha256_hex: String,
    mime_type: String,
}

fn normalize_public_avatar_image(source_path: &Path) -> OmniResult<NormalizedPublicAvatarImage> {
    normalize_avatar_image(source_path, PUBLIC_PROFILE_AVATAR_TARGET_DIMENSION)
}

/// A group's picture rides in the group's MLS meta — in every Welcome and in the Commit that sets
/// it — so it is smaller than a profile one: 128 px JPEG, a few KB.
const GROUP_AVATAR_TARGET_DIMENSION: u32 = 128;

/// The picture an admin chose for a group, normalized: (JPEG bytes, sha256 hex).
/// The profile photo as small as `budget` bytes allow: the largest of a few sizes and qualities that
/// fits, empty if even the smallest does not — for a name beacon, which must stay within a text
/// broker's message (docs/decisions/find-by-name.md).
pub fn shrink_avatar_to_budget(avatar_jpeg: &[u8], budget: usize) -> Vec<u8> {
    shrink_avatar_along(avatar_jpeg, budget, &[(48, 70), (48, 55), (40, 55), (40, 40), (32, 45), (32, 30)])
}

/// The profile photo for a member's group card (docs/decisions/group-member-profile.md): the size of
/// a group's picture, within `budget`.
pub fn shrink_avatar_for_group_card(avatar_jpeg: &[u8], budget: usize) -> Vec<u8> {
    shrink_avatar_along(avatar_jpeg, budget, &[(128, 80), (128, 65), (96, 65), (96, 50), (64, 50)])
}

fn shrink_avatar_along(avatar_jpeg: &[u8], budget: usize, steps: &[(u32, u8)]) -> Vec<u8> {
    let Ok(decoded) = image::load_from_memory(avatar_jpeg) else {
        return Vec::new();
    };
    for &(side, quality) in steps {
        let rgb = decoded.resize_to_fill(side, side, FilterType::Lanczos3).to_rgb8();
        let mut bytes = Vec::new();
        if JpegEncoder::new_with_quality(&mut bytes, quality).encode_image(&rgb).is_ok() && bytes.len() <= budget {
            return bytes;
        }
    }
    Vec::new()
}

pub fn normalize_group_avatar_image(source_path: &Path) -> OmniResult<(Vec<u8>, String)> {
    let normalized = normalize_avatar_image(source_path, GROUP_AVATAR_TARGET_DIMENSION)?;
    Ok((normalized.bytes, normalized.sha256_hex))
}

fn normalize_avatar_image(source_path: &Path, dimension: u32) -> OmniResult<NormalizedPublicAvatarImage> {
    // The pixel count is checked from the header, before decoding (it used to be checked after a
    // full decode — a small file claiming a huge picture was decoded first).
    let decoded = crate::photo::decode_picked_picture(
        source_path,
        PUBLIC_PROFILE_AVATAR_MAX_INPUT_BYTES,
        PUBLIC_PROFILE_AVATAR_MAX_INPUT_PIXELS,
    )?;

    let resized = decoded.resize_to_fill(dimension, dimension, FilterType::Lanczos3);
    let rgb = resized.to_rgb8();
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, PUBLIC_PROFILE_AVATAR_JPEG_QUALITY)
        .encode_image(&rgb)
        .map_err(|error| {
            OmniError::Storage(format!(
                "failed to encode normalized avatar as JPEG: {error}"
            ))
        })?;

    Ok(NormalizedPublicAvatarImage {
        sha256_hex: hex::encode(sha2::Sha256::digest(&bytes)),
        bytes,
        mime_type: "image/jpeg".to_string(),
    })
}

fn sanitize_public_profile_path_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn validate_non_empty_public_profile_display_name(display_name: &str) -> OmniResult<String> {
    let normalized = omni_identity::clamp_display_name(display_name);
    if normalized.is_empty() {
        return Err(OmniError::InvalidInput(
            "public profile display name must not be empty".to_string(),
        ));
    }
    Ok(normalized)
}

impl Storage {
    /// An avatar in profile media: encrypted at rest like the rest of Omni's storage
    /// (docs/decisions/db-at-rest-encryption.md, «Вложения»).
    fn write_avatar_file(&self, path: &Path, bytes: &[u8]) -> OmniResult<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                OmniError::Storage(format!(
                    "failed to create avatar directory '{}': {error}",
                    parent.display()
                ))
            })?;
        }
        self.write_media_file(path, bytes)
    }
}

fn compact_avatar_storage_key(value: &str) -> String {
    let digest = sha2::Sha256::digest(value.as_bytes());
    let full = hex::encode(digest);
    full[..16].to_string()
}
