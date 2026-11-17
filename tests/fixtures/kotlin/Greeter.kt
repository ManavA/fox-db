package demo

import demo.util.format

interface Speaker {
    fun speak(): String
}

class Greeter(private val name: String) : Speaker {
    override fun speak(): String = greet(name)

    fun greet(who: String): String {
        return format(who)
    }

    companion object {
        fun create(): Greeter = Greeter("x")
    }
}

fun main() {
    val g = Greeter.create()
    println(g.speak())
    listOf(1).map { it }
}
