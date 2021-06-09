package main

import (
	"bytes"
	"os"
	"time"

	"log"

	"github.com/vicanso/elton"
	"github.com/vicanso/elton/middleware"
)

func main() {
	staticPath := os.Getenv("STATIC")
	e := elton.New()

	e.Use(middleware.NewLogger(middleware.LoggerConfig{
		OnLog: func(s string, c *elton.Context) {
			log.Println(s)
		},
		Format: middleware.LoggerCombined,
	}))
	e.Use(middleware.NewDefaultCompress())

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
		SMaxAge:             time.Hour,
		DenyQueryString:     true,
		DisableLastModified: true,
		// 如果使用内存式文件存储，不支持Stat，因此需要用强ETag
		EnableStrongETag: true,
	}))

	err := e.ListenAndServe(":3000")
	if err != nil {
		panic(err)
	}
}
