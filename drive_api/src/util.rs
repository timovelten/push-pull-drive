use http::Request;

pub fn clone_request<T: Clone>(req: &Request<T>) -> Request<T> {
    if !req.extensions().is_empty() {
        panic!("cloning requests with extensions is unsupported")
    }

    let method = req.method().clone();
    let uri = req.uri().clone();
    let version = req.version();
    let headers = req.headers().clone();
    let body = req.body().clone();

    let mut ret = Request::builder()
        .method(method)
        .uri(uri)
        .version(version)
        .body(body)
        .unwrap();

    *ret.headers_mut() = headers;
    ret
}