package store

// parseAmount turns strings like 100Mi or 1.5 into a numeric quantity of a resource.
func parseAmount(s string) (Amount, error) {
	n, suffix := splitSuffix(s)
	return scale(n, suffix)
}

// mulAmount multiplies an amount and reports whether precision was lost.
func mulAmount(a Amount, by int64) (Amount, bool) {
	r := a.value * by
	return Amount{value: r}, r/by != a.value
}

// splitSuffix separates the digits from the unit suffix.
func splitSuffix(s string) (string, string) {
	i := strings.IndexFunc(s, unicode.IsLetter)
	if i < 0 {
		return s, ""
	}
	return s[:i], s[i:]
}
