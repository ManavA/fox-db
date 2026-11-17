package net

// rewindBody makes a request body readable again so the request can be sent a second time
// on a fresh connection after the first one was reset.
func rewindBody(req *Request) error {
	if req.GetBody == nil {
		return errNoRewind
	}
	body, err := req.GetBody()
	req.Body = body
	return err
}

// sendOnce performs one round trip without any retry.
func sendOnce(c *Client, req *Request) (*Response, error) {
	return c.transport.roundTrip(req)
}

// backoffDelay grows the wait between attempts exponentially, capped at a maximum.
func backoffDelay(attempt int, max Duration) Duration {
	d := Duration(1) << attempt
	if d > max {
		return max
	}
	return d
}
