using System.Collections.Generic;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Omni.Windows.App.Core;

internal sealed partial class RustBridge
{
    /// Agents of a workspace (docs/decisions/omni-workspace-git.md section 5): every command answers
    /// with their place as it is now.
    public WorkspaceAgents WorkspaceAgents(WorkspaceAgentsRequest request)
        => CallJsonLong(
            () => NativeMethods.omni_workspace_agents(handle,
                JsonSerializer.Serialize(request, OmniJsonSerializerContext.Default.WorkspaceAgentsRequest)),
            OmniJsonSerializerContext.Default.WorkspaceAgents, "workspace_agents");
}

/// view | stop (stopped) | answer (id, allow, reason) | diff (id, or repo and oid) | connect (code, name,
/// members_command) | forget (account_id) | give (account_id, text, task) | pair_start (task, first,
/// second, topic, rounds) | pair_close (task) | review (repo, branch, oid, accepted, tests_passed, notes) |
/// talk_list | talk_read (thread) | talk_open (topic, task) | talk_send (thread, text, reply_to) |
/// add_to_app (app: claude | codex, name, folder, exe).
internal sealed record WorkspaceAgentsRequest(
    [property: JsonPropertyName("action")] string Action,
    [property: JsonPropertyName("group_id")] string GroupId,
    [property: JsonPropertyName("stopped")] bool Stopped = false,
    [property: JsonPropertyName("id")] string Id = "",
    [property: JsonPropertyName("allow")] bool Allow = false,
    [property: JsonPropertyName("reason")] string Reason = "",
    [property: JsonPropertyName("code")] string Code = "",
    [property: JsonPropertyName("name")] string Name = "",
    [property: JsonPropertyName("members_command")] bool MembersCommand = false,
    [property: JsonPropertyName("account_id")] string AccountId = "",
    [property: JsonPropertyName("text")] string Text = "",
    [property: JsonPropertyName("task")] string? Task = null,
    [property: JsonPropertyName("first")] string First = "",
    [property: JsonPropertyName("second")] string Second = "",
    [property: JsonPropertyName("topic")] string Topic = "",
    [property: JsonPropertyName("rounds")] int? Rounds = null,
    [property: JsonPropertyName("repo")] string Repo = "",
    [property: JsonPropertyName("branch")] string Branch = "",
    [property: JsonPropertyName("oid")] string Oid = "",
    [property: JsonPropertyName("accepted")] bool Accepted = false,
    [property: JsonPropertyName("tests_passed")] bool TestsPassed = false,
    [property: JsonPropertyName("notes")] string Notes = "",
    [property: JsonPropertyName("thread")] string Thread = "",
    [property: JsonPropertyName("mentions")] List<string>? Mentions = null,
    [property: JsonPropertyName("line")] string Line = "",
    [property: JsonPropertyName("emoji")] string Emoji = "",
    [property: JsonPropertyName("reply_to")] string? ReplyTo = null,
    [property: JsonPropertyName("app")] string App = "",
    [property: JsonPropertyName("folder")] string Folder = "",
    [property: JsonPropertyName("exe")] string Exe = "");

internal sealed record WorkspaceAgents(
    [property: JsonPropertyName("my_account")] string MyAccount,
    [property: JsonPropertyName("stopped")] bool Stopped,
    [property: JsonPropertyName("can_stop")] bool CanStop,
    [property: JsonPropertyName("agents")] List<WorkspaceAgent> Agents,
    [property: JsonPropertyName("asks")] List<WorkspaceAgentAsk> Asks,
    [property: JsonPropertyName("reviews")] List<WorkspaceAgentReview> Reviews,
    [property: JsonPropertyName("turns_without_human")] int TurnsWithoutHuman,
    [property: JsonPropertyName("rounds")] int Rounds,
    [property: JsonPropertyName("stopped_by")] string? StoppedBy = null,
    // diff: what a «main» ask or a branch asked to be checked changes, whole.
    [property: JsonPropertyName("diff")] string? Diff = null,
    [property: JsonPropertyName("review_asks")] List<WorkspaceReviewAsk>? ReviewAsks = null,
    // The talk commands: the discussions, one of them whole, the one just opened.
    [property: JsonPropertyName("threads")] List<WorkspaceTalkThread>? Threads = null,
    [property: JsonPropertyName("talk")] WorkspaceTalk? Talk = null,
    [property: JsonPropertyName("opened")] string? Opened = null,
    // add_to_app: the agent's application settings file Omni was written into.
    [property: JsonPropertyName("added_to")] string? AddedTo = null);

/// A discussion of a piece of work (a task's, or of what an agent was given): people's and agents'
/// finished lines.
internal sealed record WorkspaceTalkThread(
    [property: JsonPropertyName("id")] string Id,
    [property: JsonPropertyName("topic")] string Topic,
    [property: JsonPropertyName("opened_by_name")] string OpenedByName,
    [property: JsonPropertyName("at_unix_ms")] long AtUnixMs,
    [property: JsonPropertyName("last_at_unix_ms")] long LastAtUnixMs,
    [property: JsonPropertyName("lines")] int Lines,
    [property: JsonPropertyName("agents")] List<string> Agents,
    [property: JsonPropertyName("task")] string? Task = null,
    [property: JsonPropertyName("last_author")] string? LastAuthor = null,
    [property: JsonPropertyName("last_text")] string? LastText = null);

/// A line of a discussion; command_to: the agents it is a command to; seen_by: the agents whose latest
/// line got is this one.
internal sealed record WorkspaceTalkLine(
    [property: JsonPropertyName("id")] string Id,
    [property: JsonPropertyName("author_name")] string AuthorName,
    [property: JsonPropertyName("author_account")] string AuthorAccount,
    [property: JsonPropertyName("agent")] bool Agent,
    [property: JsonPropertyName("mine")] bool Mine,
    [property: JsonPropertyName("text")] string Text,
    [property: JsonPropertyName("at_unix_ms")] long AtUnixMs,
    [property: JsonPropertyName("command_to")] List<string> CommandTo,
    [property: JsonPropertyName("owner_name")] string? OwnerName = null,
    [property: JsonPropertyName("reply_to")] string? ReplyTo = null,
    [property: JsonPropertyName("seen_by")] List<string>? SeenBy = null,
    [property: JsonPropertyName("reactions")] List<MessageReactionView>? Reactions = null);

/// One discussion whole; people: who a line here can name.
internal sealed record WorkspaceTalk(
    [property: JsonPropertyName("thread")] WorkspaceTalkThread Thread,
    [property: JsonPropertyName("lines")] List<WorkspaceTalkLine> Lines,
    [property: JsonPropertyName("people")] List<WorkspaceTalkPerson> People);

/// One the composer offers after `@`; commanded: a line from us naming them is a command to them.
internal sealed record WorkspaceTalkPerson(
    [property: JsonPropertyName("account")] string Account,
    [property: JsonPropertyName("name")] string Name,
    [property: JsonPropertyName("owner_name")] string OwnerName,
    [property: JsonPropertyName("agent")] bool Agent,
    [property: JsonPropertyName("commanded")] bool Commanded);

/// A branch its author asked to have checked, with no verdict on that commit yet; here: the commit has
/// come, so it can be read and accepted.
internal sealed record WorkspaceReviewAsk(
    [property: JsonPropertyName("repo")] string Repo,
    [property: JsonPropertyName("branch")] string Branch,
    [property: JsonPropertyName("oid")] string Oid,
    [property: JsonPropertyName("author_name")] string AuthorName,
    [property: JsonPropertyName("at_unix_ms")] long AtUnixMs,
    [property: JsonPropertyName("here")] bool Here,
    [property: JsonPropertyName("can_review")] bool CanReview,
    [property: JsonPropertyName("commits")] List<string> Commits,
    [property: JsonPropertyName("files")] List<string> Files,
    [property: JsonPropertyName("thread")] string? Thread = null);

/// An agent of the workspace, or one of ours on its way into it (state in_group | invited | connecting),
/// with what it is doing: a link to it now, its latest line, its tasks, its questions waiting.
internal sealed record WorkspaceAgent(
    [property: JsonPropertyName("account_id")] string AccountId,
    [property: JsonPropertyName("name")] string Name,
    [property: JsonPropertyName("mine")] bool Mine,
    [property: JsonPropertyName("confirmed")] bool Confirmed,
    [property: JsonPropertyName("in_group")] bool InGroup,
    [property: JsonPropertyName("state")] string State,
    [property: JsonPropertyName("members_command")] bool MembersCommand,
    [property: JsonPropertyName("owner_name")] string? OwnerName = null,
    [property: JsonPropertyName("online")] bool Online = false,
    [property: JsonPropertyName("listening")] bool Listening = false,
    [property: JsonPropertyName("last_said")] string? LastSaid = null,
    [property: JsonPropertyName("last_active_unix_ms")] long LastActiveUnixMs = 0,
    [property: JsonPropertyName("tasks")] List<string>? Tasks = null,
    [property: JsonPropertyName("waiting_answer")] int WaitingAnswer = 0);

/// An agent's question to its owner: into main (repo, branch, oid — and what it brings) or a plain
/// yes/no (text).
internal sealed record WorkspaceAgentAsk(
    [property: JsonPropertyName("id")] string Id,
    [property: JsonPropertyName("agent_name")] string AgentName,
    [property: JsonPropertyName("action")] string Action,
    [property: JsonPropertyName("text")] string Text,
    [property: JsonPropertyName("at_unix_ms")] long AtUnixMs,
    [property: JsonPropertyName("can_answer")] bool CanAnswer,
    [property: JsonPropertyName("accepted_by")] List<string> AcceptedBy,
    [property: JsonPropertyName("repo")] string? Repo = null,
    [property: JsonPropertyName("branch")] string? Branch = null,
    [property: JsonPropertyName("oid")] string? Oid = null,
    [property: JsonPropertyName("answer")] bool? Answer = null,
    [property: JsonPropertyName("reason")] string? Reason = null,
    [property: JsonPropertyName("commits")] List<string>? Commits = null,
    [property: JsonPropertyName("files")] List<string>? Files = null,
    [property: JsonPropertyName("answered_unix_ms")] long AnsweredUnixMs = 0,
    // thread: the discussion of the work it is about — a card there too.
    [property: JsonPropertyName("thread")] string? Thread = null);

/// A verdict on one exact commit of a branch.
internal sealed record WorkspaceAgentReview(
    [property: JsonPropertyName("repo")] string Repo,
    [property: JsonPropertyName("branch")] string Branch,
    [property: JsonPropertyName("oid")] string Oid,
    [property: JsonPropertyName("reviewer_name")] string ReviewerName,
    [property: JsonPropertyName("accepted")] bool Accepted,
    [property: JsonPropertyName("tests_passed")] bool TestsPassed,
    [property: JsonPropertyName("notes")] string Notes,
    [property: JsonPropertyName("at_unix_ms")] long AtUnixMs,
    [property: JsonPropertyName("thread")] string? Thread = null);
