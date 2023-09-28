use crate::DefaultRateLimiter;
use futures::stream::try_unfold;
use futures::Stream;
use http::Uri;
use octocrab::{Octocrab, Page};
use serde::de::DeserializeOwned;
use std::sync::Arc;
use tracing::{info_span, Instrument};

struct PageIterator<'octo, T> {
    crab: &'octo Octocrab,
    next: Option<Uri>,
    current: std::vec::IntoIter<T>,
    rate_limiter: Arc<DefaultRateLimiter>,
}

/// Convert Page into a stream of results
///
/// This will fetch new pages using the next link with in the page so that
/// it returns all the results that matched the original request.
///
/// E.g. iterating across all of the repos in an org with too many to fit
/// in one page of results:
/// ```no_run
/// # async fn run() -> octocrab::Result<()> {
/// use futures_util::TryStreamExt;
/// use tokio::pin;
///
/// let crab = octocrab::instance();
/// let mut stream = crab
///     .orgs("owner")
///     .list_repos()
///     .send()
///     .await?
///     .into_stream(&crab);
/// pin!(stream);
/// while let Some(repo) = stream.try_next().await? {
///     println!("{:?}", repo);
/// }
/// # Ok(())
/// # }
/// ```
pub fn into_stream<T>(
    page: Page<T>,
    crab: &Octocrab,
    rate_limiter: Arc<DefaultRateLimiter>,
) -> impl Stream<Item = crate::Result<T>> + '_
where
    T: DeserializeOwned + 'static,
{
    let state = PageIterator {
        crab,
        next: page.next,
        current: page.items.into_iter(),
        rate_limiter,
    };
    try_unfold(state, |mut state| async move {
        if let Some(val) = state.current.next() {
            return Ok(Some((val, state)));
        }
        state
            .rate_limiter
            .until_ready()
            .instrument(info_span!("wait_rate_limit"))
            .await;
        let page = state.crab.get_page::<T>(&state.next).await?;
        Ok(page.and_then(|page| {
            let mut current = page.items.into_iter();
            // If we get an empty page we'll return early here with out
            // checking next.
            // It doesn't really make much sense to have an empty page in
            // the middle so we assume this isn't going to happen
            let val = current.next()?;
            let state = PageIterator {
                crab: state.crab,
                next: page.next,
                current,
                rate_limiter: state.rate_limiter,
            };
            Some((val, state))
        }))
    })
}
