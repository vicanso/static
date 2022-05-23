package main

import (
	"bytes"
	"os"
	"regexp"
	"strconv"
	"time"

	"log"

	"github.com/vicanso/elton"
	"github.com/vicanso/elton/middleware"
)

func main() {
	staticPath := os.Getenv("STATIC")
	compressLevel, _ := strconv.Atoi(os.Getenv("CMP_LEVEL"))
	minLength, _ := strconv.Atoi(os.Getenv("CMP_MIN_LENGTH"))
	checker, _ := regexp.Compile(os.Getenv("CMP_CONTENT_TYPE"))
	e := elton.New()

	e.Use(middleware.NewLogger(middleware.LoggerConfig{
		OnLog: func(s string, _ *elton.Context) {
			log.Println(s)
		},
		Format: middleware.LoggerCombined,
	}))
	e.Use(middleware.NewDefaultFresh())
	e.Use(middleware.NewDefaultETag())
	if compressLevel != 0 {
		config := middleware.NewCompressConfig(
			&middleware.BrCompressor{
				MinLength: minLength,
				Level:     compressLevel,
			},
			&middleware.GzipCompressor{
				MinLength: minLength,
				Level:     compressLevel,
			},
		)
		config.Checker = checker
		e.Use(middleware.NewCompress(config))
	}

	sf := new(middleware.FS)
	e.GET("/ping", func(c *elton.Context) error {
		c.BodyBuffer = bytes.NewBufferString("pong")
		return nil
	})
	e.GET("/", func(c *elton.Context) (err error) {
		r, err := sf.NewReader(staticPath + "/index.html")
		if err != nil {
			return
		}
		c.SetContentTypeByExt(".html")
		c.Body = r
		return
	})
	// static file route
	e.GET("/*", middleware.NewStaticServe(sf, middleware.StaticServeConfig{
		Path: staticPath,
		// 客户端缓存一年
		MaxAge: 365 * 24 * time.Hour,
		// 缓存服务器缓存一个小时
		SMaxAge: time.Hour,
		// 禁止访问隐藏文件
		DenyDot: true,
	}))

	err := e.ListenAndServe(":3000")
	if err != nil {
		panic(err)
	}
}
