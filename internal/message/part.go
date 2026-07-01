package message

// Part is implemented only by the concrete part kinds declared within this
// package. The unexported isPart method closes the interface so no external
// package can satisfy it, keeping the union exhaustive at compile time.
type Part interface {
	Type() string
	isPart()
}

// Parts is a named slice of Part with a dedicated JSON codec.
type Parts []Part
