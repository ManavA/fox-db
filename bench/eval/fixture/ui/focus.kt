package ui

/** Keeps the focused row in the middle of the screen while the list scrolls vertically. */
fun centerFocusedRow(list: RowList, focused: Int) {
    val target = list.height / 2 - list.rowHeight(focused) / 2
    list.scrollTo(focused, target)
}

/** Draws a band at the top that hides content scrolling under the navigation bar. */
fun topEdgeScrim(canvas: Canvas, height: Int) {
    canvas.gradient(0, height)
}
