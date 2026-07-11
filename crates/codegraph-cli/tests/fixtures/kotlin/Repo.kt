class Api {
    fun fetch(): Int = 1
}
class Maker {
    fun build(): Int = 2
}
class Repo(val api: Api) {
    fun load(): Int {
        val m = Maker()
        return api.fetch() + m.build()
    }
}

class Runner {
    fun exec(block: () -> Int): Int = block()
}
class Caller {
    fun run(): Int {
        val r = Runner()
        return r.exec {
            val inLambda = Maker()
            inLambda.build()
        }
    }
}
