use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::sync::Arc;

use axum::{
    http::StatusCode,
    Json,
    Router, routing::get,
};
use axum::extract::{Path, Query, State};
use dotenvy_macro::dotenv;
use markdown::mdast::Node;
use markdown::ParseOptions;
use octocrab::Octocrab;
use serde::Serialize;
use tokio::sync::Mutex;
use uluru::LRUCache;

const GITHUB_PAT: &'static str = dotenv!("GITHUB_AT");

type CacheState = Arc<Mutex<LRUCache<ApiResponse,8192>>>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // set our default instance to use github PAT
    let crab = Octocrab::builder()
        .personal_token(GITHUB_PAT.to_string())
        .build()?;
    octocrab::initialise(crab);

    let state: CacheState = Arc::new(Mutex::new(LRUCache::new()));


    let app = Router::new()
        .route("/:org/:repo", get(get_release_notes))
        .route("/force/:org/:repo", get(force_refresh))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:4200").await?;

    axum::serve(listener, app).await?;

    Ok(())
}

async fn force_refresh(
    Path((org, repo)): Path<(String,String)>,
    Query(params): Query<HashMap<String,String>>,
    State(state): State<CacheState>
) -> StatusCode {

    let mut cache = state.lock().await;
    let is_latest = params.get("tag").is_none()
        || params.get("tag").is_some_and(|s| s.as_str() == "latest");
    let tag = params.get("tag");
    let octocrab = octocrab::instance();
    let repos = octocrab.repos(org.clone(), repo.clone());
    let releases = repos.releases();

    let release = match tag {
        Some(tag) => releases.get_by_tag(tag).await.map_err(|e| {
            eprintln!("{}", e);
            StatusCode::NOT_FOUND
        }),
        _ => releases.get_latest().await.map_err(|e| {
            eprintln!("{}", e);
            StatusCode::NOT_FOUND
        })
    };
    match release {
        Ok(release) => {
            cache.insert(ApiResponse {
                repo,
                org,
                latest: is_latest,
                title: release.name.unwrap_or(release.tag_name.clone()),
                author: release.author.map(|a| AuthorInfo {
                    name: a.login,
                    image: a.avatar_url.to_string()
                }),
                tag: release.tag_name,
                items: Item::from_list(release.body),
                url: release.html_url.to_string(),
            });
            StatusCode::OK
        },
        _ => StatusCode::INTERNAL_SERVER_ERROR
    }
}


async fn get_release_notes(
    Path((org, repo)): Path<(String,String)>,
    Query(params): Query<HashMap<String,String>>,
    State(state): State<CacheState>
) -> Result<Json<ApiResponse>, StatusCode> {
    let release: Result<ApiResponse,StatusCode> = {

        let mut cache = state.lock().await;

        // if the 'tag' param is nothing or the literal "latest" then fetch latest
        let fetch_latest = params.get("tag").is_none()
            || params.get("tag").is_some_and(|s| s.as_str() == "latest");
        let tag = params.get("tag");

        let result = match cache.find(|res| res.org == org && res.repo == repo && (fetch_latest == res.latest || tag.is_some_and(|t| t == &res.tag))) {
            Some(release) => Ok::<ApiResponse,StatusCode>(release.clone()),
            None => {
                let octocrab = octocrab::instance();
                let repos = octocrab.repos(org.clone(), repo.clone());
                let releases = repos.releases();

                let release = match tag {
                    Some(tag) => releases.get_by_tag(tag).await.map_err(|e| {
                        eprintln!("{}", e);
                        StatusCode::NOT_FOUND
                    })?,
                    _ => releases.get_latest().await.map_err(|e| {
                        eprintln!("{}", e);
                        StatusCode::NOT_FOUND
                    })?
                };
                let response = ApiResponse {
                    repo,
                    org,
                    latest: fetch_latest,
                    title: release.name.unwrap_or(release.tag_name.clone()),
                    author: release.author.map(|a| AuthorInfo {
                        name: a.login,
                        image: a.avatar_url.to_string()
                    }),
                    tag: release.tag_name,
                    items: Item::from_list(release.body),
                    url: release.html_url.to_string(),
                };
                cache.insert(response.clone()); // actually put in cache
                Ok(response)
            }
        };
        result
    };

    match release {
        Ok(res) => Ok(Json(res)),
        Err(e) => Err(e)
    }
}

#[derive(Serialize, Debug, Clone)]
pub struct AuthorInfo {
    pub name: String,
    pub image: String,
}

#[derive(Serialize, Debug, Clone)]
pub struct Item {
    pub category: String,
    pub text: String,
}

impl Item {
    fn from_list(body: Option<String>) -> Vec<Self> {
        match body {
            None => vec![],
            Some(notes) => {
                let ast = markdown::to_mdast(notes.as_str(), &ParseOptions::gfm());
                match ast {
                    Ok(node) => {
                        match Self::build_items(&node, None) {
                            Some(items) => Self::reduce_ast(items),
                            _ => vec![]
                        }
                    },
                    Err(e) => {
                        vec![]
                    }
                }
            }
        }
    }

    fn reduce_ast(items: Vec<Item>) -> Vec<Item> {
        let mut item_queue = VecDeque::from(items);
        let mut transformed = vec![];
        let mut building = String::new();
        while let Some(next) = item_queue.pop_front() {
            if next.category.starts_with("break") {
                transformed.push(Item {
                    category: "text".to_string(),
                    text: building.clone()
                });
                building.clear();
                continue;
            }
            match next.category.as_str() {
                "italic" => {
                    building.push_str("<i>");
                    building.push_str(next.text.as_str());
                    building.push_str("</i>");
                },
                "bold" => {
                    building.push_str("<b>");
                    building.push_str(next.text.as_str());
                    building.push_str("</b>");
                },
                _ => building.push_str(next.text.as_str())
            }
        }
        transformed
    }

    fn build_items(node: &Node, context: Option<&Node>) -> Option<Vec<Self>> {
        match node {
            Node::Root(root) => {
                Some(root.children.iter().filter_map(|i| Self::build_items(i, Some(node))).flat_map(|i|i).collect())
            },
            Node::Paragraph(paragraph) => {
                let break_item = Item {
                    category: "break-p".to_string(),
                    text: "".to_string()
                };
                if paragraph.children.len() == 1 && paragraph.children.first().is_some_and(|n| n.type_id() == (&Node::Image).type_id()) {
                    None
                } else {
                    Some(paragraph.children.iter().filter_map(|i| Self::build_items(i, Some(node))).flat_map(|i|i).chain([break_item]).collect())
                }
            },
            Node::List(list) => {
                let break_item = Item {
                    category: "break-l".to_string(),
                    text: "".to_string()
                };
                Some(list.children.iter().filter_map(|i| Self::build_items(i, Some(node))).flat_map(|i|i).chain([break_item]).collect())
            },
            Node::ListItem(item) => {
                Some(item.children.iter().filter_map(|i| Self::build_items(i, Some(node))).flat_map(|i|i).collect())
            },
            Node::Strong(strong) => {
                Some(strong.children.iter().filter_map(|i| Self::build_items(i, Some(node))).flat_map(|i|i).collect())
            },
            Node::Link(link) => {
                Some(link.children.iter().filter_map(|i| Self::build_items(i, Some(node))).flat_map(|i|i).collect())
            },
            Node::Emphasis(italic) => {
                Some(italic.children.iter().filter_map(|i| Self::build_items(i, Some(node))).flat_map(|i|i).collect())
            },
            Node::InlineCode(code) => {
                Some(vec![Item {
                    category: "code".to_string(),
                    text: code.value.clone()
                }])
            }
            Node::Text(text) => {
                let text_type = match context {
                    Some(Node::Strong(_)) => "bold",
                    Some(Node::Emphasis(_)) => "italic",
                    Some(Node::Link(link)) => link.url.as_str(),
                    _ => "text"
                };
                Some(vec![Item {
                    category: text_type.to_string(),
                    text: text.value.clone(),
                }])
            },
            _ => None
        }
    }

}


#[derive(Serialize, Debug, Clone)]
pub struct ApiResponse {
    pub repo: String,
    pub org: String,
    pub title: String,
    pub latest: bool,
    pub author: Option<AuthorInfo>,
    pub tag: String,
    pub items: Vec<Item>,
    pub url: String,
}
