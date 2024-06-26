use anyhow::Context;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use slack_morphism::prelude::*;
use strum::Display;
use tracing::instrument;
use url::Url;

#[derive(Deserialize, Debug)]
pub struct User {
    pub email: String,
    pub username: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
pub enum PullRequestState {
    Open,
    Closed,
}

#[derive(Deserialize, Debug)]
pub struct Repository {
    pub full_name: String,
}

#[derive(Deserialize, Debug)]
pub struct Comment {
    pub body: String,
}

#[derive(Deserialize, Debug)]
pub struct PullRequest {
    pub body: String,
    pub comments: u64,
    pub id: u64,
    pub user: User,
    pub title: String,
    #[serde(rename = "html_url")]
    pub url: Url,
    pub state: PullRequestState,
}

#[derive(Deserialize, Debug, Display)]
#[serde(tag = "type")]
#[strum(serialize_all = "snake_case")]
pub enum Review {
    #[serde(rename = "pull_request_review_approved")]
    Approved { content: String },
    #[serde(rename = "pull_request_review_rejected")]
    Rejected { content: String },
    #[serde(rename = "pull_request_review_comment")]
    #[strum(serialize = "commented on")]
    Comment { content: String },
}

#[derive(Deserialize, Debug, Display)]
#[serde(rename_all = "snake_case", tag = "action")]
#[strum(serialize_all = "snake_case")]
pub enum Action {
    Opened,
    Closed,
    Reopened,
    Merged,
    Created { comment: Comment },
    Reviewed { review: Review },
    ReviewRequested { requested_reviewer: User },
}

#[derive(Deserialize, Debug)]
pub struct Webhook {
    #[serde(flatten)]
    pub action: Action,
    #[serde(alias = "issue")]
    pub pull_request: PullRequest,
    pub sender: User,
    pub repository: Repository,
}

#[derive(Serialize, Debug)]
pub struct OutgoingWebhook {
    pub email: String,
    pub title: String,
    pub body: String,
}

pub struct MySlackMessage<'a> {
    pub webhook: &'a Webhook,
    pub slack_user: Vec<SlackUser>,
}

impl Webhook {
    pub async fn try_deanonymise_emails(mut self) -> Self {
        /* Setting the path is the easiest way to keep the scheme and host together but remove the path */
        let mut url = self.pull_request.url.clone();

        /* If the email can't be de-anonymised for some reason, keep the anon email */
        if let Ok(email) = Webhook::fetch_gitea_user_email(&mut url, &self.sender.username).await {
            self.sender.email = email;
        }

        if let Ok(email) =
            Webhook::fetch_gitea_user_email(&mut url, &self.pull_request.user.username).await
        {
            self.pull_request.user.email = email;
        }

        if let Action::ReviewRequested {
            ref mut requested_reviewer,
        } = self.action
        {
            if let Ok(email) =
                Webhook::fetch_gitea_user_email(&mut url, &requested_reviewer.username).await
            {
                requested_reviewer.email = email;
            }
        }

        self
    }

    #[instrument(err)]
    async fn fetch_gitea_user_email(
        url: &mut Url,
        username: &str,
    ) -> Result<String, anyhow::Error> {
        let token = config_env_var("GITEA_API_TOKEN")?;

        url.set_path(format!("api/v1/users/{}", username).as_str());

        let res = Client::new()
            .get(url.as_str())
            .header("Authorization", "token ".to_string() + &token.to_owned())
            .send()
            .await?
            .json::<User>()
            .await?;

        Ok(res.email)
    }

    async fn into_my_slack(&self) -> Option<MySlackMessage> {
        let emails = match self.action {
            Action::ReviewRequested {
                ref requested_reviewer,
            } => vec![requested_reviewer.email.clone()],
            Action::Reviewed { review: _ } => vec![self.pull_request.user.email.clone()],
            Action::Created { ref comment } => {
                Webhook::parse_comment_for_mention(&self.pull_request.url, comment).await
            }
            _ => Vec::new(),
        };

        let mut slack_users = Vec::<Option<SlackUser>>::new();
        for email in emails {
            slack_users.push(Webhook::fetch_slack_user_from_email(&email).await.ok());
        }

        let slack_user: Vec<SlackUser> = slack_users.into_iter().filter_map(|x| x).collect();

        if let Action::Created { comment: _ } = self.action {
            if slack_user.len() == 0 {
                return None;
            }
        }

        Some(MySlackMessage {
            webhook: self,
            slack_user,
        })
    }

    async fn parse_comment_for_mention(url: &Url, comment: &Comment) -> Vec<String> {
        let users = comment
            .body
            .lines()
            .filter_map(|line| {
                let line = line.trim_start();
                if line.starts_with(">") {
                    None
                } else {
                    Some(line)
                }
            })
            .flat_map(|x| x.split_whitespace())
            .filter_map(|x| {
                if x.starts_with("@") {
                    Some(x.trim_start_matches("@"))
                } else {
                    None
                }
            });

        let mut mention_emails = Vec::<String>::new();
        for user in users {
            if let Some(email) = Webhook::fetch_gitea_user_email(&mut url.clone(), user)
                .await
                .ok()
            {
                mention_emails.push(email);
            }
        }

        mention_emails
    }

    #[instrument(err)]
    async fn fetch_slack_user_from_email(email: &str) -> Result<SlackUser, anyhow::Error> {
        let client = SlackClient::new(SlackClientHyperConnector::new()?);
        let token_value: SlackApiTokenValue = config_env_var("SLACK_API_TOKEN")?.into();
        let token = SlackApiToken::new(token_value);
        let session = client.open_session(&token);

        let request = SlackApiUsersLookupByEmailRequest::new(EmailAddress(email.to_string()));
        let slack_user = session.users_lookup_by_email(&request).await?;

        Ok(slack_user.user)
    }

    #[instrument(err)]
    pub async fn post_slack_message(
        &self,
        parent: &Option<SlackTs>,
    ) -> Result<SlackTs, anyhow::Error> {
        let client = SlackClient::new(SlackClientHyperConnector::new()?);
        let token_value: SlackApiTokenValue = config_env_var("SLACK_API_TOKEN")?.into();
        let token = SlackApiToken::new(token_value);
        let session = client.open_session(&token);

        let message = self
            .into_my_slack()
            .await
            .context("Unable to convert")?
            .render_template();

        let channel = config_env_var("SLACK_CHANNEL")?;

        let post_chat_req = if let Some(thread_ts) = parent {
            SlackApiChatPostMessageRequest::new(channel.into(), message)
                .with_thread_ts(thread_ts.clone())
        } else {
            SlackApiChatPostMessageRequest::new(channel.into(), message)
        };

        let post_chat_resp = session.chat_post_message(&post_chat_req).await?;

        Ok(post_chat_resp.ts)
    }
}

impl SlackMessageTemplate for MySlackMessage<'_> {
    fn render_template(&self) -> SlackMessageContent {
        match &self.webhook.action {
            Action::Opened => render_pr_opened(&self.webhook),
            Action::Reviewed { review } => render_reviewed(self, review),
            Action::ReviewRequested { requested_reviewer } => {
                render_review_requested(self, &requested_reviewer)
            }
            Action::Created { comment: _ } => render_comment(self),
            _ => render_basic_action(&self.webhook),
        }
    }
}

fn format_pull_request_url(pull_request: &PullRequest) -> String {
    format!("<{}|{}>", pull_request.url, pull_request.title)
}

fn render_basic_action(webhook: &Webhook) -> SlackMessageContent {
    SlackMessageContent::new().with_blocks(slack_blocks![some_into(
        SlackSectionBlock::new().with_text(md!(
            "{} was {}",
            format_pull_request_url(&webhook.pull_request),
            webhook.action
        ))
    )])
}

fn render_comment(slack_message: &MySlackMessage) -> SlackMessageContent {
    let mentions = slack_message
        .slack_user
        .clone()
        .into_iter()
        .map(|x| x.id.to_slack_format())
        .collect::<Vec<String>>()
        .join(" ");

    SlackMessageContent::new().with_blocks(slack_blocks![some_into(
        SlackSectionBlock::new().with_text(md!("{}, you were mentioned in a comment", mentions))
    )])
}

fn render_reviewed(slack_message: &MySlackMessage, review: &Review) -> SlackMessageContent {
    let user = if let Some(user) = slack_message.slack_user.first() {
        user.id.to_slack_format()
    } else {
        slack_message.webhook.pull_request.user.username.to_string()
    };

    SlackMessageContent::new().with_blocks(slack_blocks![some_into(
        SlackSectionBlock::new().with_text(md!(
            "{}, {} has {} your PR",
            user,
            slack_message.webhook.sender.username,
            review
        ))
    )])
}

fn render_review_requested(slack_message: &MySlackMessage, reviewer: &User) -> SlackMessageContent {
    let user = if let Some(user) = slack_message.slack_user.first() {
        user.id.to_slack_format()
    } else {
        reviewer.username.to_string()
    };

    SlackMessageContent::new().with_blocks(slack_blocks![some_into(
        SlackSectionBlock::new().with_text(md!(
            "{}, {} has requested you to review {}",
            user,
            slack_message.webhook.sender.username,
            format_pull_request_url(&slack_message.webhook.pull_request)
        ))
    )])
}

fn render_pr_opened(webhook: &Webhook) -> SlackMessageContent {
    let repo_name = webhook
        .repository
        .full_name
        .split_once("/")
        .expect("Invalid full_name field!");

    let body = webhook
        .pull_request
        .body
        .split_inclusive("\n")
        .map(|line| ">".to_string() + line)
        .collect::<Vec<String>>()
        .join("");

    SlackMessageContent::new().with_blocks(slack_blocks![
        some_into(SlackHeaderBlock::new(pt!(
            "{} | {}",
            repo_name.0,
            repo_name.1
        ))),
        some_into(SlackSectionBlock::new().with_text(md!(
            "Pull request {} opened by {}",
            format_pull_request_url(&webhook.pull_request),
            webhook.sender.username
        ))),
        some_into(SlackSectionBlock::new().with_text(md!("{}", body)))
    ])
}

fn config_env_var(name: &str) -> Result<String, anyhow::Error> {
    Ok(std::env::var(name)?)
}
