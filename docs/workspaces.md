# Working in a workspace

This walkthrough uses a folder of documents and the labels in the Windows app. Start with a small folder containing a file you want to share with the workspace members.

## Share a folder and save a version

1. Open **Workspaces**, choose **+ Workspace** and select your folder. You can also drop the folder onto the workspace list.
2. When Omni asks what the folder contains, choose **Файлы** (files). Omni names the workspace after the folder, adds its first project and saves the first version. Wait for the confirmation, especially with large files.
3. Edit a file in that folder and save it in your editor. In Omni, open **Проекты** and select the project. Review the listed changes and choose **Сохранить версию**. Open **История** to check that the new version appears.

## Give an agent a task and review the result

1. Open "Агенты". If you have no agent yet, choose "Подключить агента" and follow the connection instructions for your agent client.
2. On your agent’s card, choose **Дать работу…**. Select an existing board task or describe a new one. For a first task, ask the agent to read a named file and reply with a one-sentence summary. Send the assignment.
3. Read the reply in the discussion opened for the assignment. To return to it later, open **Обсуждения**. If the agent was disconnected, it can pick up the assignment when it reconnects.
4. For an assignment that changes files, open the project under **Проекты**. Check **История** for a saved version or **Предложения** if the agent submitted a proposal. Inspect the affected files before accepting proposed changes.

## If you are connecting an MCP client

`omni_wait` can return the same batch again after an interruption. Use `(group_id, message_id)` to recognize a line you already processed. Process every line sharing a receipt, then call `omni_ack` with that group's `group_id` and `receipt`. Acknowledgement is safe to retry. Send the answer with `omni_send` using the assignment’s `thread_id` so the person who assigned it sees the reply in the same discussion.

See [workspace trust boundaries](../omni-threat-model.md#workspace-trust-boundaries) for who can give an agent commands.
