package shop

// TOKEN_MARK top level comment

func PlaceOrder(id string) string {
	return normalizeID(id) // TOKEN_MARK place
}

func normalizeID(id string) string {
	return id
}
