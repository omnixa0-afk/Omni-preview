using System.Collections.Generic;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Omni.Windows.App.Core;

internal sealed partial class RustBridge
{
    public WorkspaceFilesDetails WorkspaceFiles(WorkspaceFilesRequest request)
        => CallJsonLong(
            () => NativeMethods.omni_workspace_files(handle,
                JsonSerializer.Serialize(request, OmniJsonSerializerContext.Default.WorkspaceFilesRequest)),
            OmniJsonSerializerContext.Default.WorkspaceFilesDetails, "workspace_files");
}

/// One typed boundary for the file-workspace commands. Rust validates the fields required by each
/// action and the authenticated profile's authority; the UI never supplies a role or sender.
internal sealed record WorkspaceFilesRequest(
    [property: JsonPropertyName("action")] string Action,
    [property: JsonPropertyName("group_id")] string GroupId,
    [property: JsonPropertyName("repo")] string Repo = "",
    [property: JsonPropertyName("account_id")] string AccountId = "",
    [property: JsonPropertyName("nomination")] string Nomination = "",
    [property: JsonPropertyName("accepted")] bool Accepted = false,
    [property: JsonPropertyName("folder")] string? Folder = null,
    [property: JsonPropertyName("participants_need_approval")] bool ParticipantsNeedApproval = true,
    [property: JsonPropertyName("newcomers_download_current")] bool NewcomersDownloadCurrent = false,
    [property: JsonPropertyName("oid")] string? Oid = null,
    [property: JsonPropertyName("path")] string Path = "",
    [property: JsonPropertyName("target")] string Target = "",
    // release: the revision's designation and note; keep: «Сохранить навсегда» or taking it back.
    [property: JsonPropertyName("label")] string Label = "",
    [property: JsonPropertyName("note")] string Note = "",
    [property: JsonPropertyName("keep")] bool Keep = false,
    // thinning (the report) and retain (confirming the rule): the last versions and days kept.
    [property: JsonPropertyName("keep_last")] uint KeepLast = 0,
    [property: JsonPropertyName("keep_days")] uint KeepDays = 0,
    // …and the masks of files kept only in the current version (`*.stl`).
    [property: JsonPropertyName("only_current")] List<string>? OnlyCurrent = null,
    // leave_out: the project's folders this device does not take into its working folder.
    [property: JsonPropertyName("folders")] List<string>? Folders = null,
    // take / give: the files «Беру в работу» / «Отдать».
    [property: JsonPropertyName("paths")] List<string>? Paths = null,
    // policy: files are edited only once taken; everything else is read-only in the folder.
    [property: JsonPropertyName("edit_only_taken")] bool EditOnlyTaken = false,
    // policy: нормоконтроль — a revision counts once another reviewer approves it.
    [property: JsonPropertyName("revisions_checked")] bool RevisionsChecked = false,
    // choose: which of the file (`path`) and its conflict copies stays as the file.
    [property: JsonPropertyName("chosen")] string Chosen = "",
    // task_new: the title and description; task_change / task_describe / task_comment: which task.
    [property: JsonPropertyName("title")] string? Title = null,
    [property: JsonPropertyName("body")] string? Body = null,
    [property: JsonPropertyName("task")] string? Task = null,
    [property: JsonPropertyName("text")] string? Text = null,
    // task_new / task_change: the fields set; null: left as they are (assignee "": nobody).
    [property: JsonPropertyName("task_title")] string? TaskTitle = null,
    [property: JsonPropertyName("status")] string? Status = null,
    [property: JsonPropertyName("assignee")] string? Assignee = null,
    [property: JsonPropertyName("labels")] List<string>? Labels = null,
    [property: JsonPropertyName("links")] List<string>? Links = null,
    // task_change: dragged right above this task of the column ("": to its end).
    [property: JsonPropertyName("before")] string? Before = null,
    // model_view: how the model is turned (degrees) and the picture's size (pixels).
    [property: JsonPropertyName("yaw")] float Yaw = 0,
    [property: JsonPropertyName("pitch")] float Pitch = 0,
    [property: JsonPropertyName("width")] uint Width = 0,
    [property: JsonPropertyName("height")] uint Height = 0);

internal sealed record WorkspaceFilesPolicy(
    [property: JsonPropertyName("participants_need_approval")] bool ParticipantsNeedApproval,
    [property: JsonPropertyName("newcomers_download_current")] bool NewcomersDownloadCurrent,
    [property: JsonPropertyName("changed_at_unix_ms")] long ChangedAtUnixMs,
    [property: JsonPropertyName("edit_only_taken")] bool EditOnlyTaken = false,
    [property: JsonPropertyName("revisions_checked")] bool RevisionsChecked = false);

internal sealed record WorkspaceCustodian(
    [property: JsonPropertyName("account_id")] string AccountId,
    [property: JsonPropertyName("name")] string Name,
    [property: JsonPropertyName("admin")] bool Admin,
    [property: JsonPropertyName("nominated")] bool Nominated,
    [property: JsonPropertyName("nomination")] string? Nomination,
    [property: JsonPropertyName("keeper")] bool Keeper,
    [property: JsonPropertyName("is_me")] bool IsMe,
    [property: JsonPropertyName("storing_here")] bool StoringHere,
    [property: JsonPropertyName("copy_complete")] bool CopyComplete = false,
    [property: JsonPropertyName("copy_missing_bytes")] long? CopyMissingBytes = null);

internal sealed record WorkspaceFile(
    [property: JsonPropertyName("path")] string Path,
    [property: JsonPropertyName("bytes")] long Bytes,
    [property: JsonPropertyName("available_here")] bool AvailableHere,
    // Against the shared version: accepted | changed (in the folder, not saved) | saved (in a version
    // of ours the shared one lacks).
    [property: JsonPropertyName("state")] string State = "accepted",
    // False: only we have it, nothing to download.
    [property: JsonPropertyName("in_version")] bool InVersion = true,
    // In a folder this device leaves out of its working folder.
    [property: JsonPropertyName("left_out")] bool LeftOut = false,
    // «Беру в работу»: who holds the file now.
    [property: JsonPropertyName("lock")] WorkspaceLock? Lock = null,
    // We took it too, but someone else first.
    [property: JsonPropertyName("taken_after")] bool TakenAfter = false);

/// Who holds a file («🔒 Иван, до 31.10 10:00»).
internal sealed record WorkspaceLock(
    [property: JsonPropertyName("account_id")] string AccountId,
    [property: JsonPropertyName("name")] string Name,
    [property: JsonPropertyName("mine")] bool Mine,
    [property: JsonPropertyName("since_unix_ms")] long SinceUnixMs,
    [property: JsonPropertyName("until_unix_ms")] long UntilUnixMs);

/// A top folder of the project's shared version.
internal sealed record WorkspaceFolder(
    [property: JsonPropertyName("path")] string Path,
    [property: JsonPropertyName("bytes")] long Bytes);

internal sealed record WorkspaceProposal(
    [property: JsonPropertyName("account_id")] string AccountId,
    [property: JsonPropertyName("name")] string Name,
    [property: JsonPropertyName("oid")] string Oid,
    [property: JsonPropertyName("subject")] string Subject,
    [property: JsonPropertyName("changed")] List<string> Changed,
    [property: JsonPropertyName("stale")] bool Stale,
    [property: JsonPropertyName("accepted")] bool Accepted,
    [property: JsonPropertyName("rejected")] bool Rejected);

internal sealed record WorkspaceFilesDetails(
    [property: JsonPropertyName("policy")] WorkspaceFilesPolicy Policy,
    [property: JsonPropertyName("i_am_admin")] bool IAmAdmin,
    [property: JsonPropertyName("can_review")] bool CanReview,
    [property: JsonPropertyName("members")] List<WorkspaceCustodian> Members,
    [property: JsonPropertyName("files")] List<WorkspaceFile> Files,
    [property: JsonPropertyName("proposals")] List<WorkspaceProposal> Proposals,
    [property: JsonPropertyName("work_path")] string? WorkPath,
    [property: JsonPropertyName("root")] string? Root,
    [property: JsonPropertyName("suggested_folder")] string? SuggestedFolder,
    [property: JsonPropertyName("folder")] string? Folder,
    [property: JsonPropertyName("history_bytes")] long HistoryBytes,
    [property: JsonPropertyName("history_missing_bytes")] long HistoryMissingBytes,
    [property: JsonPropertyName("current_bytes")] long CurrentBytes,
    [property: JsonPropertyName("single_copy")] bool SingleCopy,
    [property: JsonPropertyName("download_status")] string? DownloadStatus,
    [property: JsonPropertyName("size_known")] bool SizeKnown = false,
    [property: JsonPropertyName("complete_copies")] int CompleteCopies = 0,
    [property: JsonPropertyName("history_path")] string? HistoryPath = null,
    // Filled by the `history` action only: accepted versions, newest first.
    [property: JsonPropertyName("history")] List<WorkspaceHistoryEntry>? History = null,
    [property: JsonPropertyName("next_revision")] string NextRevision = "",
    // The retention rule: the confirmed one, or the default offered while none is.
    [property: JsonPropertyName("keep_last")] uint KeepLast = 0,
    [property: JsonPropertyName("keep_days")] uint KeepDays = 0,
    [property: JsonPropertyName("only_current")] List<string>? OnlyCurrent = null,
    [property: JsonPropertyName("retention_confirmed")] bool RetentionConfirmed = false,
    // Filled by the `thinning` action only.
    [property: JsonPropertyName("thinning")] WorkspaceThinningReport? Thinning = null,
    // The folders this device leaves out, and the project's top folders to choose from.
    [property: JsonPropertyName("left_out")] List<string>? LeftOut = null,
    [property: JsonPropertyName("folders")] List<WorkspaceFolder>? Folders = null,
    // Filled by `file_history`: the accepted versions that changed the file, newest first.
    [property: JsonPropertyName("file_history")] List<WorkspaceFileVersion>? FileHistory = null,
    // Filled by `preview`: where the file's content is here, or that it is on its way.
    [property: JsonPropertyName("preview")] WorkspacePreview? Preview = null,
    // Filled by `export`: the file under its own name, to drag out or open (or that it is on its way).
    [property: JsonPropertyName("export")] WorkspacePreview? Export = null,
    // The shared version's files kept with conflict copies beside them («выбрать итоговую»).
    [property: JsonPropertyName("conflicts")] List<WorkspaceConflict>? Conflicts = null,
    // Filled by the task actions: the board after them.
    [property: JsonPropertyName("tasks")] WorkspaceTasks? Tasks = null,
    // Filled by `model_view`: an STL model drawn by the core (PNG), or that it is on its way.
    [property: JsonPropertyName("model_view")] WorkspaceModelView? ModelView = null);

internal sealed record WorkspaceModelView(
    [property: JsonPropertyName("png_base64")] string? PngBase64,
    [property: JsonPropertyName("pending")] bool Pending,
    [property: JsonPropertyName("triangles")] int Triangles);

/// The workspace's board: tasks are files of its tasks project.
internal sealed record WorkspaceTasks(
    [property: JsonPropertyName("enabled")] bool Enabled,
    [property: JsonPropertyName("can_enable")] bool CanEnable,
    [property: JsonPropertyName("folder")] string? Folder,
    [property: JsonPropertyName("tasks")] List<WorkspaceTask> Tasks,
    [property: JsonPropertyName("members")] List<WorkspaceTaskMember> Members);

internal sealed record WorkspaceTaskMember(
    [property: JsonPropertyName("account_id")] string AccountId,
    [property: JsonPropertyName("name")] string Name,
    [property: JsonPropertyName("is_me")] bool IsMe);

/// A task: #N, its fields (status new | doing | review | done), comments and the versions naming it.
internal sealed record WorkspaceTask(
    [property: JsonPropertyName("id")] string Id,
    [property: JsonPropertyName("number")] int Number,
    [property: JsonPropertyName("title")] string Title,
    [property: JsonPropertyName("status")] string Status,
    [property: JsonPropertyName("assignee")] string? Assignee,
    [property: JsonPropertyName("assignee_name")] string? AssigneeName,
    [property: JsonPropertyName("labels")] List<string> Labels,
    [property: JsonPropertyName("links")] List<string> Links,
    [property: JsonPropertyName("body")] string Body,
    [property: JsonPropertyName("author_name")] string AuthorName,
    [property: JsonPropertyName("created_unix_ms")] long CreatedUnixMs,
    [property: JsonPropertyName("updated_unix_ms")] long UpdatedUnixMs,
    [property: JsonPropertyName("comments")] List<WorkspaceTaskComment> Comments,
    [property: JsonPropertyName("mentions")] List<WorkspaceTaskMention> Mentions,
    [property: JsonPropertyName("conflict")] bool Conflict,
    // «Проработать вдвоём»: the task's latest session of two agents.
    [property: JsonPropertyName("pair")] WorkspacePair? Pair = null,
    // Its place in its column, the smaller the higher.
    [property: JsonPropertyName("place")] double Place = 0);

/// Two agents working a task out (status working | agreed | deadlock | limit).
internal sealed record WorkspacePair(
    [property: JsonPropertyName("session")] string Session,
    [property: JsonPropertyName("topic")] string Topic,
    [property: JsonPropertyName("with")] List<string> With,
    [property: JsonPropertyName("rounds_max")] int RoundsMax,
    [property: JsonPropertyName("round")] int Round,
    [property: JsonPropertyName("status")] string Status,
    [property: JsonPropertyName("drafts")] List<WorkspacePairDraft> Drafts,
    [property: JsonPropertyName("remarks")] List<WorkspacePairRemark> Remarks,
    [property: JsonPropertyName("agreed_by")] List<string> AgreedBy);

internal sealed record WorkspacePairDraft(
    [property: JsonPropertyName("file")] string File,
    [property: JsonPropertyName("round")] int Round,
    [property: JsonPropertyName("author_name")] string AuthorName,
    [property: JsonPropertyName("at_unix_ms")] long AtUnixMs,
    [property: JsonPropertyName("text")] string Text);

internal sealed record WorkspacePairRemark(
    [property: JsonPropertyName("id")] string Id,
    [property: JsonPropertyName("on")] string On,
    [property: JsonPropertyName("author_name")] string AuthorName,
    [property: JsonPropertyName("severity")] string Severity,
    [property: JsonPropertyName("what")] string What,
    [property: JsonPropertyName("why")] string Why,
    [property: JsonPropertyName("taken")] bool? Taken,
    [property: JsonPropertyName("answer_why")] string? AnswerWhy = null);

internal sealed record WorkspaceTaskComment(
    [property: JsonPropertyName("author_name")] string AuthorName,
    [property: JsonPropertyName("at_unix_ms")] long AtUnixMs,
    [property: JsonPropertyName("text")] string Text,
    [property: JsonPropertyName("mine")] bool Mine);

/// A version whose comment named the task («#12»).
internal sealed record WorkspaceTaskMention(
    [property: JsonPropertyName("repo")] string Repo,
    [property: JsonPropertyName("oid")] string Oid,
    [property: JsonPropertyName("subject")] string Subject,
    [property: JsonPropertyName("author_name")] string AuthorName,
    [property: JsonPropertyName("at_unix_ms")] long AtUnixMs);

/// A file and the conflict copies kept beside it when two saves changed it.
internal sealed record WorkspaceConflict(
    [property: JsonPropertyName("path")] string Path,
    [property: JsonPropertyName("copies")] List<WorkspaceConflictCopy> Copies);

/// A conflict copy: its path and «Петя, 30.10 10-42» — whose change it is and when.
internal sealed record WorkspaceConflictCopy(
    [property: JsonPropertyName("path")] string Path,
    [property: JsonPropertyName("label")] string Label);

/// An accepted version that changed one file; no size: it removed the file.
internal sealed record WorkspaceFileVersion(
    [property: JsonPropertyName("number")] int Number,
    [property: JsonPropertyName("oid")] string Oid,
    [property: JsonPropertyName("subject")] string Subject,
    [property: JsonPropertyName("author")] string Author,
    [property: JsonPropertyName("at_unix_ms")] long AtUnixMs,
    [property: JsonPropertyName("bytes")] long? Bytes = null,
    [property: JsonPropertyName("revision")] string? Revision = null,
    [property: JsonPropertyName("revision_pending")] bool RevisionPending = false);

/// A file of a version to show.
internal sealed record WorkspacePreview(
    [property: JsonPropertyName("path")] string Path,
    [property: JsonPropertyName("oid")] string Oid,
    [property: JsonPropertyName("bytes")] long Bytes,
    [property: JsonPropertyName("file")] string? File = null,
    [property: JsonPropertyName("pending")] bool Pending = false);

/// What a retention rule would take away against the confirmed one.
internal sealed record WorkspaceThinningReport(
    [property: JsonPropertyName("keep_last")] uint KeepLast,
    [property: JsonPropertyName("keep_days")] uint KeepDays,
    [property: JsonPropertyName("versions")] List<WorkspaceHistoryEntry> Versions,
    [property: JsonPropertyName("freed_bytes")] long FreedBytes,
    [property: JsonPropertyName("restored_versions")] int RestoredVersions,
    [property: JsonPropertyName("only_current")] List<string>? OnlyCurrent = null,
    // Versions that would keep all but their files under the masks.
    [property: JsonPropertyName("partial_versions")] int PartialVersions = 0);

/// An accepted version in the project's history.
internal sealed record WorkspaceHistoryEntry(
    [property: JsonPropertyName("number")] int Number,
    [property: JsonPropertyName("oid")] string Oid,
    [property: JsonPropertyName("subject")] string Subject,
    [property: JsonPropertyName("author")] string Author,
    [property: JsonPropertyName("at_unix_ms")] long AtUnixMs,
    [property: JsonPropertyName("changed")] int Changed,
    [property: JsonPropertyName("revision")] string? Revision,
    [property: JsonPropertyName("revision_note")] string RevisionNote,
    [property: JsonPropertyName("kept")] bool Kept,
    [property: JsonPropertyName("current")] bool Current,
    [property: JsonPropertyName("thinned")] bool Thinned = false,
    // Kept without the content of its files under the «only the current» masks.
    [property: JsonPropertyName("partial")] bool Partial = false,
    // «На нормоконтроле»: released under the rule, not approved yet; we released it.
    [property: JsonPropertyName("revision_pending")] bool RevisionPending = false,
    [property: JsonPropertyName("revision_mine")] bool RevisionMine = false);
