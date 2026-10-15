use super::{PagedClDiffItem, PatchSections, collect_page_blobs, prelude::*};

impl super::MonoApiService {
    /// Fetches the content difference for a merge request, paginated by page_id and page_size.
    /// # Arguments
    /// * `cl_link` - The link to the merge request.
    /// * `page_id` - The page number to fetch. (id out of bounds will return empty)
    /// * `page_size` - The number of items per page.
    /// # Returns
    ///  a `Result` containing `ClDiff` on success or a `GitError` on failure.
    /// Build paged CL diff items with optional relocation metadata for CL views.
    pub(in crate::ceres::api_service::mono_api_service) async fn paged_content_diff_items(
        &self,
        cl_link: &str,
        page: Pagination,
    ) -> Result<(Vec<PagedClDiffItem>, u64), GitError> {
        let per_page = page.per_page as usize;
        let page_id = page.page as usize;

        let stg = self.storage.cl_storage();
        let cl =
            stg.get_cl(cl_link).await.unwrap().ok_or_else(|| {
                GitError::CustomError(format!("Merge request not found: {cl_link}"))
            })?;
        let old_blobs = self
            .get_commit_blobs(&cl.from_hash)
            .await
            .map_err(|e| GitError::CustomError(format!("Failed to get old commit blobs: {e}")))?;
        let new_blobs = self
            .get_commit_blobs(&cl.to_hash)
            .await
            .map_err(|e| GitError::CustomError(format!("Failed to get new commit blobs: {e}")))?;

        let sorted_changed_files = self
            .cl_files_list(old_blobs, new_blobs)
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;

        let start = (page_id.saturating_sub(1)) * per_page;
        let end = (start + per_page).min(sorted_changed_files.len());

        let page_slice: &[ClDiffFile] = if start < sorted_changed_files.len() {
            let start_idx = start;
            let end_idx = end;
            &sorted_changed_files[start_idx..end_idx]
        } else {
            &[]
        };

        let non_relocated_items: Vec<ClDiffFile> = page_slice
            .iter()
            .filter(|item| {
                !matches!(
                    item,
                    ClDiffFile::Renamed(_, _, _, _, _) | ClDiffFile::Moved(_, _, _, _, _)
                )
            })
            .cloned()
            .collect();

        let mut page_old_blobs = Vec::new();
        let mut page_new_blobs = Vec::new();
        collect_page_blobs(
            &non_relocated_items,
            &mut page_old_blobs,
            &mut page_new_blobs,
        );

        let raw_diff_output = if non_relocated_items.is_empty() {
            Vec::new()
        } else {
            self.get_diff_by_blobs(page_old_blobs, page_new_blobs)
                .await?
        };

        let mut raw_diff_by_path: HashMap<String, Vec<DiffItem>> = HashMap::new();
        for item in raw_diff_output {
            raw_diff_by_path
                .entry(item.path.clone())
                .or_default()
                .push(item);
        }

        let mut diff_output: Vec<PagedClDiffItem> = Vec::with_capacity(page_slice.len());
        for item in page_slice {
            match item {
                ClDiffFile::Renamed(old_path, new_path, old_hash, new_hash, similarity)
                | ClDiffFile::Moved(old_path, new_path, old_hash, new_hash, similarity) => {
                    diff_output.push(PagedClDiffItem {
                        item: self
                            .format_relocated_diff_item(
                                old_path,
                                new_path,
                                *old_hash,
                                *new_hash,
                                *similarity,
                            )
                            .await?,
                        old_path: Some(old_path.to_string_lossy().replace('\\', "/")),
                    });
                }
                _ => {
                    let key = item.path().to_string_lossy().replace('\\', "/");
                    if let Some(items) = raw_diff_by_path.get_mut(&key)
                        && !items.is_empty()
                    {
                        diff_output.push(PagedClDiffItem {
                            item: items.remove(0),
                            old_path: None,
                        });
                    }
                }
            }
        }

        let total = sorted_changed_files.len().div_ceil(per_page);

        Ok((diff_output, total as u64))
    }

    /// Return the legacy paged diff shape without CL-specific metadata.
    pub async fn paged_content_diff(
        &self,
        cl_link: &str,
        page: Pagination,
    ) -> Result<(Vec<DiffItem>, u64), GitError> {
        let (items, total) = self.paged_content_diff_items(cl_link, page).await?;
        Ok((items.into_iter().map(|item| item.item).collect(), total))
    }

    /// Return paged diff items tailored for the CL files-changed API.
    pub async fn paged_content_diff_for_cl(
        &self,
        cl_link: &str,
        page: Pagination,
    ) -> Result<(Vec<ClFilesChangedItemSchema>, u64), GitError> {
        let (items, total) = self.paged_content_diff_items(cl_link, page).await?;
        Ok((
            items
                .into_iter()
                .map(|item| ClFilesChangedItemSchema::new(item.item, item.old_path))
                .collect(),
            total,
        ))
    }

    pub(in crate::ceres::api_service::mono_api_service) async fn get_diff_by_blobs(
        &self,
        old_blobs: Vec<(PathBuf, ObjectHash)>,
        new_blobs: Vec<(PathBuf, ObjectHash)>,
    ) -> Result<Vec<DiffItem>, GitError> {
        let mut blob_cache: HashMap<ObjectHash, Vec<u8>> = HashMap::new();

        // Collect all unique hashes
        let mut all_hashes = HashSet::new();
        for (_, hash) in &old_blobs {
            all_hashes.insert(*hash);
        }
        for (_, hash) in &new_blobs {
            all_hashes.insert(*hash);
        }

        // Fetch all blobs with better error handling and logging
        let mut failed_hashes = Vec::new();
        for hash in all_hashes {
            match self.get_raw_blob_by_hash(&hash.to_string()).await {
                Ok(data) => {
                    blob_cache.insert(hash, data);
                }
                Err(e) => {
                    tracing::error!("Failed to fetch blob {}: {}", hash, e);
                    failed_hashes.push(hash);
                    blob_cache.insert(hash, Vec::new());
                }
            }
        }

        if !failed_hashes.is_empty() {
            tracing::warn!(
                "Failed to fetch {} blob(s): {:?}",
                failed_hashes.len(),
                failed_hashes
            );
        }

        // Enhanced content reader with better error handling
        let read_content = |file: &PathBuf, hash: &ObjectHash| -> Vec<u8> {
            match blob_cache.get(hash) {
                Some(content) => content.clone(),
                None => {
                    tracing::warn!("Missing blob content for file: {:?}, hash: {}", file, hash);
                    Vec::new()
                }
            }
        };

        // Use the unified diff function with configurable algorithm
        let diff_output = GitDiff::diff(old_blobs, new_blobs, Vec::new(), read_content)
            .into_iter()
            .map(Self::normalize_diff_item)
            .collect();

        Ok(diff_output)
    }

    pub(in crate::ceres::api_service::mono_api_service) async fn format_relocated_diff_item(
        &self,
        old_path: &Path,
        new_path: &Path,
        old_hash: ObjectHash,
        new_hash: ObjectHash,
        similarity: u8,
    ) -> Result<DiffItem, GitError> {
        let mut patch = Self::format_relocated_patch_header(old_path, new_path, similarity);

        if old_hash != new_hash {
            let raw_items = self
                .get_diff_by_blobs(
                    vec![(old_path.to_path_buf(), old_hash)],
                    vec![(old_path.to_path_buf(), new_hash)],
                )
                .await?;
            if let Some(item) = raw_items.into_iter().next() {
                patch.push_str(&Self::relocate_patch_body(&item.data, old_path, new_path));
            }
        }

        Ok(DiffItem {
            path: new_path.to_string_lossy().replace('\\', "/"),
            data: patch,
        })
    }

    pub(in crate::ceres::api_service::mono_api_service) fn format_relocated_patch_header(
        old_path: &Path,
        new_path: &Path,
        similarity: u8,
    ) -> String {
        let old_path = old_path.to_string_lossy().replace('\\', "/");
        let new_path = new_path.to_string_lossy().replace('\\', "/");
        format!(
            "diff --git a/{old_path} b/{new_path}\nsimilarity index {similarity}%\nrename from {old_path}\nrename to {new_path}\n"
        )
    }

    pub(in crate::ceres::api_service::mono_api_service) fn normalize_diff_item(
        mut item: DiffItem,
    ) -> DiffItem {
        item.path = item.path.replace('\\', "/");
        item.data = Self::normalize_patch_header_paths(&item.data);
        item
    }

    pub(in crate::ceres::api_service::mono_api_service) fn normalize_patch_header_paths(
        raw_patch: &str,
    ) -> String {
        let sections = Self::split_patch_sections(raw_patch);
        let header_lines = sections
            .header_lines
            .into_iter()
            .map(Self::normalize_patch_header_line)
            .collect();
        let divider_line = sections.divider_line.map(|line| {
            if line.starts_with("Binary files ") {
                line.replace('\\', "/")
            } else {
                line.to_string()
            }
        });

        Self::join_patch_sections(
            header_lines,
            divider_line,
            sections.payload_lines,
            sections.has_trailing_newline,
        )
    }

    pub(in crate::ceres::api_service::mono_api_service) fn relocate_patch_body(
        raw_patch: &str,
        old_path: &Path,
        new_path: &Path,
    ) -> String {
        let old_path = old_path.to_string_lossy().replace('\\', "/");
        let new_path = new_path.to_string_lossy().replace('\\', "/");
        let sections = Self::split_patch_sections(raw_patch);
        let header_lines = sections
            .header_lines
            .into_iter()
            .filter(|line| !line.starts_with("diff --git "))
            .map(|line| {
                if line.starts_with("--- a/") {
                    format!("--- a/{old_path}")
                } else if line.starts_with("+++ b/") {
                    format!("+++ b/{new_path}")
                } else {
                    line.to_string()
                }
            })
            .collect();
        let divider_line = sections.divider_line.map(|line| {
            if line.starts_with("Binary files ") {
                format!("Binary files a/{old_path} and b/{new_path} differ")
            } else {
                line.to_string()
            }
        });

        Self::join_patch_sections(
            header_lines,
            divider_line,
            sections.payload_lines,
            sections.has_trailing_newline,
        )
    }

    pub(in crate::ceres::api_service::mono_api_service) fn normalize_patch_header_line(
        line: &str,
    ) -> String {
        if line.starts_with("diff --git ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("rename from ")
            || line.starts_with("rename to ")
        {
            line.replace('\\', "/")
        } else {
            line.to_string()
        }
    }

    pub(in crate::ceres::api_service::mono_api_service) fn split_patch_sections(
        raw_patch: &str,
    ) -> PatchSections<'_> {
        let mut header_lines = Vec::new();
        let mut divider_line = None;
        let mut payload_lines = Vec::new();
        let mut in_payload = false;

        for line in raw_patch.lines() {
            if in_payload {
                payload_lines.push(line);
                continue;
            }

            if line.starts_with("@@")
                || line.starts_with("Binary files ")
                || line.starts_with("GIT binary patch")
            {
                divider_line = Some(line);
                in_payload = true;
                continue;
            }

            header_lines.push(line);
        }

        PatchSections {
            header_lines,
            divider_line,
            payload_lines,
            has_trailing_newline: raw_patch.ends_with('\n'),
        }
    }

    pub(in crate::ceres::api_service::mono_api_service) fn join_patch_sections(
        header_lines: Vec<String>,
        divider_line: Option<String>,
        payload_lines: Vec<&str>,
        has_trailing_newline: bool,
    ) -> String {
        let mut lines = header_lines;
        if let Some(line) = divider_line {
            lines.push(line);
        }
        lines.extend(payload_lines.into_iter().map(String::from));

        let rendered = lines.join("\n");
        if rendered.is_empty() {
            rendered
        } else if has_trailing_newline {
            format!("{rendered}\n")
        } else {
            rendered
        }
    }

    pub async fn get_sorted_changed_file_list(
        &self,
        cl_link: &str,
        path: Option<&str>,
    ) -> Result<Vec<String>, MegaError> {
        let normalized_prefix = path.map(|prefix| prefix.replace('\\', "/"));
        // UN-19: this runs on the merge path now, so a storage failure must
        // propagate rather than panic the request.
        let cl = self
            .storage
            .cl_storage()
            .get_cl(cl_link)
            .await?
            .ok_or_else(|| MegaError::Other(format!("CL not found: {cl_link}")))?;

        let old_files = self.get_commit_blobs(&cl.from_hash.clone()).await?;
        let new_files = self.get_commit_blobs(&cl.to_hash.clone()).await?;

        // calculate pages
        let sorted_changed_files = self.cl_files_list(old_files, new_files).await?;
        let file_paths: Vec<String> = sorted_changed_files
            .iter()
            .map(|f| f.path().to_string_lossy().replace('\\', "/"))
            .filter(|file_path| {
                if let Some(prefix) = &normalized_prefix {
                    file_path.starts_with(prefix)
                } else {
                    true
                }
            })
            .collect();

        Ok(file_paths)
    }
}
