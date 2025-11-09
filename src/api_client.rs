use reqwest::{Client, RequestBuilder, Result, Url};

pub struct ApiClient {
    client: Client,
    base_url: Url,
    token: String,
}

impl ApiClient {
    pub fn new(base_url: Url, token: String) -> Self {
        ApiClient {
            client: Client::new(),
            base_url,
            token,
        }
    }

    pub fn get(&self, endpoint: &str) -> RequestBuilder {
        let url = self.base_url.join(endpoint).expect("Invalid endpoint");
        self.client
            .get(url)
            .header("Authorization", format!("Bearer {}", self.token))
    }

    pub fn post(&self, endpoint: &str) -> RequestBuilder {
        let url = self.base_url.join(endpoint).expect("Invalid endpoint");
        self.client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
    }
}
