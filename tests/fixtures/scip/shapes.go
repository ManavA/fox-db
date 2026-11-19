package shapes

// Shape has an area.
type Shape interface {
	Area() int
}

// Square is a Shape.
type Square struct{ Side int }

func (s Square) Area() int { return s.Side * s.Side }

// Total adds the areas of
// every shape it is given.
func Total(xs []Shape) int {
	n := 0
	for _, x := range xs {
		n += x.Area()
	}
	return n
}

func useTotal() int {
	return Total([]Shape{Square{Side: 2}})
}
