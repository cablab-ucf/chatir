buildscript {
    repositories {
        google()
        mavenCentral()
    }
    dependencies {
        classpath("com.android.tools.build:gradle:9.3.1")
        classpath("org.jetbrains.kotlin:kotlin-gradle-plugin:2.2.10")
    }
}

allprojects {
    repositories {
        google()
        mavenCentral()
    }
}

// pushir's library leaves Kotlin to AGP 9, which Tauri's plugins switch off
// (gradle.properties: android.builtInKotlin=false): its plugin, from here.
project(":pushir") {
    plugins.withId("com.android.library") {
        apply(plugin = "org.jetbrains.kotlin.android")
        tasks.withType<org.jetbrains.kotlin.gradle.tasks.KotlinCompile>().configureEach {
            compilerOptions.jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17)
        }
    }
}

tasks.register("clean").configure {
    delete("build")
}

