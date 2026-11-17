package net

// joinPathKeepSlash joins URL path segments and keeps a trailing slash when the last segment had one.
func joinPathKeepSlash(elem ...string) string {
	out := path.Join(elem...)
	if strings.HasSuffix(elem[len(elem)-1], "/") && !strings.HasSuffix(out, "/") {
		out += "/"
	}
	return out
}

// requestBody returns the request payload bytes.
func requestBody(req *Request) []byte {
	return req.payload
}
