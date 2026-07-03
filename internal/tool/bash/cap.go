package bash

import "bytes"

// cappedBuffer is an io.Writer that accumulates up to limit bytes and
// silently discards anything written beyond that, tracking whether
// truncation occurred so Text can append a notice.
type cappedBuffer struct {
	buf       bytes.Buffer
	limit     int
	truncated bool
}

// newCappedBuffer returns a cappedBuffer that retains at most limit bytes.
func newCappedBuffer(limit int) *cappedBuffer {
	return &cappedBuffer{limit: limit}
}

// Write appends up to the remaining capacity of p to the buffer, discarding
// the rest. It always reports (len(p), nil): returning a non-nil error would
// abort the exec.Cmd copy goroutine and SIGPIPE the child process.
func (c *cappedBuffer) Write(p []byte) (int, error) {
	remaining := c.limit - c.buf.Len()
	if remaining <= 0 {
		if len(p) > 0 {
			c.truncated = true
		}
		return len(p), nil
	}

	if len(p) > remaining {
		c.buf.Write(p[:remaining])
		c.truncated = true
		return len(p), nil
	}

	c.buf.Write(p)
	return len(p), nil
}

// Text returns the buffered content, with a truncation notice appended if
// any input was discarded.
func (c *cappedBuffer) Text() string {
	if c.truncated {
		return c.buf.String() + "\n[output truncated after 100 KiB]"
	}
	return c.buf.String()
}
